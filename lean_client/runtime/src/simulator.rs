//! Deterministic simulator with per-service FIFO queues.
//!
//! Each service owns a FIFO queue. Within a service, messages are always
//! processed in arrival order. What *is* non-deterministic is **which service**
//! runs its next message at any given step — chosen by a seeded PRNG so that
//! different seeds explore different interleavings.
//!
//! # Scheduling model
//!
//! ```text
//! push_event(event)
//!   └─ appends ServiceInput::Event(event) to every service's queue
//!
//! decisions()
//!   └─ returns [ServiceId] for every service with a non-empty queue
//!
//! step()
//!   └─ picks one ServiceId from decisions() uniformly at random
//!      pops the front item from that service's queue
//!      processes it → outputs are appended to the relevant queues
//!      returns Some(ServiceId)
//! ```
//!
//! # Example
//!
//! ```ignore
//! let mut sim = Simulator::new(store, None, None);
//!
//! sim.push_event(Event::Tick(Tick::new(0, Interval::BlockProposal)));
//!
//! while sim.has_pending() {
//!     sim.step();
//!     assert!(/* store invariant */);
//! }
//!
//! let effects = sim.drain_effects();
//! ```

use fork_choice::Store;
use rand::Rng as _;
use rand_chacha::{rand_core::SeedableRng as _, ChaCha8Rng};
use std::collections::VecDeque;
use tracing::warn;

use crate::{
    chain::{ChainMessage, ChainService},
    environment::{Effect, Event, Message, Service, ServiceInput, ServiceOutput},
    network::{NetworkEffect, NetworkEvent, NetworkMessage},
    validator::{KeyManager, ValidatorConfig, ValidatorMessage, ValidatorService},
};

/// Identifies which service owns a queue slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceId {
    Chain,
    Validator,
}

/// Effect emitted by a specific node in a simulated cluster.
#[derive(Debug, Clone)]
pub struct NodeEffect {
    pub node_index: usize,
    pub effect: Effect,
}

/// Deterministic multi-node simulator.
///
/// This orchestrates multiple [`Simulator`] instances and routes network
/// effects between nodes:
///
/// - `PublishBlock` / `PublishAttestation` are broadcast to all *other* nodes
///   as `Event::Network`.
/// - `RequestBlocksByRoot` is currently recorded as an effect but not
///   auto-fulfilled.
#[derive(Clone)]
pub struct ClusterSimulator {
    nodes: Vec<Simulator>,
    effects: Vec<NodeEffect>,
    rng: ChaCha8Rng,
}

impl ClusterSimulator {
    /// Create a multi-node simulator with the built-in default seed.
    pub fn new(nodes: Vec<Simulator>) -> Self {
        Self::with_seed(nodes, 0)
    }

    /// Create a multi-node simulator with a specific seed.
    pub fn with_seed(nodes: Vec<Simulator>, seed: u64) -> Self {
        Self {
            nodes,
            effects: Vec::new(),
            rng: ChaCha8Rng::seed_from_u64(seed),
        }
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn store(&self, node_index: usize) -> &Store {
        self.nodes[node_index].store()
    }

    /// Returns `true` if at least one node has pending service inputs.
    pub fn has_pending(&self) -> bool {
        self.nodes.iter().any(Simulator::has_pending)
    }

    /// Which node indices currently have pending service inputs.
    pub fn decisions(&self) -> Vec<usize> {
        self.nodes
            .iter()
            .enumerate()
            .filter_map(|(idx, node)| node.has_pending().then_some(idx))
            .collect()
    }

    /// Broadcast an event to every node.
    pub fn push_event(&mut self, event: Event) {
        for node in &mut self.nodes {
            node.push_event(event.clone());
        }
    }

    /// Process one pending item from one randomly selected node.
    ///
    /// Returns the selected node index, or `None` when all nodes are idle.
    pub fn step(&mut self) -> Option<usize> {
        let decisions = self.decisions();
        if decisions.is_empty() {
            return None;
        }

        let idx = self.rng.random_range(0..decisions.len());
        let node_index = decisions[idx];

        self.nodes[node_index]
            .step()
            .expect("chosen node has pending items");

        let produced_effects = self.nodes[node_index].drain_effects();
        for effect in produced_effects {
            self.route_effect(node_index, &effect);
            self.effects.push(NodeEffect { node_index, effect });
        }

        Some(node_index)
    }

    /// Runs at most `max_steps` steps, stopping early when idle.
    ///
    /// Returns `true` if the cluster became idle before reaching `max_steps`.
    pub fn run_until_idle(&mut self, max_steps: usize) -> bool {
        for _ in 0..max_steps {
            if self.step().is_none() {
                return true;
            }
        }

        !self.has_pending()
    }

    /// Take all node effects accumulated since the last call.
    pub fn drain_effects(&mut self) -> Vec<NodeEffect> {
        std::mem::take(&mut self.effects)
    }

    fn route_effect(&mut self, source_node: usize, effect: &Effect) {
        match effect {
            Effect::Network(NetworkEffect::PublishBlock(block)) => {
                for (node_index, node) in self.nodes.iter_mut().enumerate() {
                    if node_index == source_node {
                        continue;
                    }
                    node.push_event(Event::Network(NetworkEvent::GossipBlock(block.clone())));
                }
            }
            Effect::Network(NetworkEffect::PublishAttestation(attestation)) => {
                for (node_index, node) in self.nodes.iter_mut().enumerate() {
                    if node_index == source_node {
                        continue;
                    }
                    node.push_event(Event::Network(NetworkEvent::GossipAttestation(
                        attestation.clone(),
                    )));
                }
            }
            Effect::Network(NetworkEffect::RequestBlocksByRoot(_))
            | Effect::Network(NetworkEffect::SendRequestBlocksByRoot { .. })
            | Effect::Network(NetworkEffect::SendResponse { .. }) => {}
        }
    }
}

#[derive(Clone)]
pub struct Simulator {
    chain: ChainService,
    chain_queue: VecDeque<ServiceInput<ChainMessage>>,
    validator: Option<ValidatorService>,
    validator_queue: VecDeque<ServiceInput<ValidatorMessage>>,
    /// Effects accumulated since the last `drain_effects` call.
    effects: Vec<Effect>,
    rng: ChaCha8Rng,
}

impl Simulator {
    /// Create a simulator with the built-in default seed.
    pub fn new(
        store: Store,
        validator_config: Option<ValidatorConfig>,
        key_manager: Option<KeyManager>,
    ) -> Self {
        Self::with_seed(store, validator_config, key_manager, 0)
    }

    /// Create a simulator with a specific seed for reproducible interleavings.
    pub fn with_seed(
        store: Store,
        validator_config: Option<ValidatorConfig>,
        key_manager: Option<KeyManager>,
        seed: u64,
    ) -> Self {
        let validator = validator_config
            .zip(key_manager)
            .map(|(config, manager)| ValidatorService::new(config, manager));

        Self {
            chain: ChainService::new(store),
            chain_queue: VecDeque::new(),
            validator,
            validator_queue: VecDeque::new(),
            effects: Vec::new(),
            rng: ChaCha8Rng::seed_from_u64(seed),
        }
    }

    pub fn store(&self) -> &Store {
        self.chain.store()
    }

    pub fn chain_service(&self) -> &ChainService {
        &self.chain
    }

    /// Returns `true` if any service has at least one queued item.
    pub fn has_pending(&self) -> bool {
        !self.chain_queue.is_empty()
            || (!self.validator_queue.is_empty() && self.validator.is_some())
    }

    /// Which services have at least one queued item.
    pub fn decisions(&self) -> Vec<ServiceId> {
        let mut ids = Vec::new();
        if !self.chain_queue.is_empty() {
            ids.push(ServiceId::Chain);
        }
        if self.validator.is_some() && !self.validator_queue.is_empty() {
            ids.push(ServiceId::Validator);
        }
        ids
    }

    /// Broadcast an event to every configured service's queue.
    pub fn push_event(&mut self, event: Event) {
        self.chain_queue
            .push_back(ServiceInput::Event(event.clone()));
        if self.validator.is_some() {
            self.validator_queue.push_back(ServiceInput::Event(event));
        }
    }

    /// Randomly pick one service from `decisions()` and process its next item.
    ///
    /// Outputs are appended to the relevant service queues (FIFO order
    /// preserved). Returns the chosen `ServiceId`, or `None` if all queues
    /// are empty.
    pub fn step(&mut self) -> Option<ServiceId> {
        let decisions = self.decisions();
        if decisions.is_empty() {
            return None;
        }

        let service_id = if self.should_prioritize_chain_tick() {
            ServiceId::Chain
        } else {
            let idx = self.rng.random_range(0..decisions.len());
            decisions[idx]
        };

        let out = match service_id {
            ServiceId::Chain => {
                let item = self
                    .chain_queue
                    .pop_front()
                    .expect("non-empty by decisions()");
                self.chain.handle_input(item)
            }
            ServiceId::Validator => {
                let item = self
                    .validator_queue
                    .pop_front()
                    .expect("non-empty by decisions()");
                match &mut self.validator {
                    Some(v) => v.handle_input(item),
                    None => {
                        warn!("validator queue item dropped: no validator configured");
                        ServiceOutput::none()
                    }
                }
            }
        };

        for message in out.messages {
            match message {
                Message::Chain(msg) => {
                    self.chain_queue.push_back(ServiceInput::Message(msg));
                }
                Message::Validator(msg) => {
                    self.validator_queue.push_back(ServiceInput::Message(msg));
                }
                Message::Network(msg) => match msg {
                    NetworkMessage::RequestBlocksByRoot(block_roots) => {
                        self.effects
                            .push(Effect::Network(NetworkEffect::RequestBlocksByRoot(
                                block_roots,
                            )));
                    }
                    NetworkMessage::SendStatusResponse { .. }
                    | NetworkMessage::SendBlocksByRootChunk { .. } => {}
                },
            }
        }

        self.effects.extend(out.effects);

        Some(service_id)
    }

    fn should_prioritize_chain_tick(&self) -> bool {
        let chain_tick = matches!(
            self.chain_queue.front(),
            Some(ServiceInput::Event(Event::Tick(_)))
        );
        let validator_tick = matches!(
            self.validator_queue.front(),
            Some(ServiceInput::Event(Event::Tick(_)))
        );

        chain_tick && validator_tick
    }

    /// Take all effects accumulated since the last call.
    pub fn drain_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clock::{Interval, Tick};
    use containers::{Block, BlockBody, Slot, State, Validator};
    use rand_chacha::rand_core::SeedableRng as _;
    use ssz::{SszHash as _, SszWrite as _, H256};
    use tempfile::TempDir;
    use xmss::SecretKey;

    use super::*;

    const NODE_COUNT: u64 = 3;
    const VALIDATOR_COUNT: u64 = 3;
    const SLOTS_TO_SIMULATE: u64 = 12;
    const MAX_STEPS_PER_TICK: usize = 200_000;

    #[test]
    fn three_simulated_nodes_finalize_chain() {
        std::thread::Builder::new()
            .name("simulator-finalization".to_string())
            .stack_size(64 * 1024 * 1024)
            .spawn(run_three_simulated_nodes_finalize_chain)
            .expect("simulation thread should start")
            .join()
            .expect("simulation thread should not panic");
    }

    fn run_three_simulated_nodes_finalize_chain() {
        let (validators, keys_dir) = generate_validators_and_keys(VALIDATOR_COUNT);

        let nodes = (0..NODE_COUNT)
            .map(|node_index| {
                create_node_simulator(keys_dir.path(), &validators, node_index, Some(node_index))
            })
            .collect();

        let mut cluster = ClusterSimulator::with_seed(nodes, 11);

        for slot in 1..=SLOTS_TO_SIMULATE {
            for interval in [
                Interval::BlockProposal,
                Interval::AttestationBroadcast,
                Interval::Aggregation,
                Interval::SafeTargetUpdate,
                Interval::AttestationAcceptance,
            ] {
                cluster.push_event(Event::Tick(Tick::new(slot, interval)));
                assert!(
                    cluster.run_until_idle(MAX_STEPS_PER_TICK),
                    "cluster did not become idle after slot {slot} / interval {interval:?}",
                );
            }
        }

        for node_index in 0..cluster.node_count() {
            let finalized_slot = cluster.store(node_index).latest_finalized().slot;
            assert!(
                finalized_slot > Slot(0),
                "node {node_index} did not finalize (latest finalized slot: {})",
                finalized_slot.0,
            );
        }
    }

    fn generate_validators_and_keys(count: u64) -> (Vec<Validator>, TempDir) {
        let key_dir = tempfile::tempdir().expect("temporary key directory should be created");
        let mut rng = ChaCha8Rng::seed_from_u64(999);
        let mut validators = Vec::with_capacity(count as usize);

        for validator_index in 0..count {
            let (public_key, secret_key) = SecretKey::generate_key_pair(&mut rng, 0, 32);

            validators.push(Validator {
                pubkey: public_key,
                index: validator_index,
            });

            let key_path = key_dir
                .path()
                .join(format!("validator_{validator_index}_sk.ssz"));
            let bytes = secret_key
                .to_ssz()
                .expect("validator secret key should serialize");

            std::fs::write(&key_path, bytes)
                .expect("validator secret key should be written to disk");
        }

        (validators, key_dir)
    }

    fn create_node_simulator(
        key_dir: &Path,
        validators: &[Validator],
        node_index: u64,
        validator_index: Option<u64>,
    ) -> Simulator {
        let state = State::generate_genesis_with_validators(1_000, validators.to_vec());
        let genesis_block = Block {
            slot: Slot(0),
            proposer_index: 0,
            parent_root: H256::zero(),
            state_root: state.hash_tree_root(),
            body: BlockBody::default(),
        };

        let store = Store::new(state, genesis_block, Some(node_index))
            .expect("simulator store init should succeed");

        let (validator_config, key_manager) = if let Some(validator_index) = validator_index {
            (
                Some(ValidatorConfig {
                    validator_indices: vec![validator_index],
                }),
                Some(
                    KeyManager::load(key_dir, &[validator_index])
                        .expect("validator key should load successfully"),
                ),
            )
        } else {
            (None, None)
        };

        Simulator::with_seed(store, validator_config, key_manager, node_index + 1)
    }
}
