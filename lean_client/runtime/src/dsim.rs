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
//! let mut sim = DeterministicSimulator::new(store, None, None);
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

use std::collections::VecDeque;

use fork_choice::store::Store;
use rand::Rng as _;
use rand_chacha::{ChaCha8Rng, rand_core::SeedableRng as _};
use tracing::warn;

use crate::{
    chain::{ChainMessage, ChainService},
    simulation::{Effect, Event, Message, Service, ServiceInput, ServiceOutput},
    validator::{KeyManager, ValidatorConfig, ValidatorMessage, ValidatorService},
};

/// Identifies which service owns a queue slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceId {
    Chain,
    Validator,
}

#[derive(Clone)]
pub struct DeterministicSimulator {
    chain: ChainService,
    chain_queue: VecDeque<ServiceInput<ChainMessage>>,
    validator: Option<ValidatorService>,
    validator_queue: VecDeque<ServiceInput<ValidatorMessage>>,
    /// Effects accumulated since the last `drain_effects` call.
    effects: Vec<Effect>,
    rng: ChaCha8Rng,
}

impl DeterministicSimulator {
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
            self.validator_queue
                .push_back(ServiceInput::Event(event));
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

        let idx = self.rng.random_range(0..decisions.len());
        let service_id = decisions[idx];

        let out = match service_id {
            ServiceId::Chain => {
                let item = self.chain_queue.pop_front().expect("non-empty by decisions()");
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
            }
        }

        self.effects.extend(out.effects);

        Some(service_id)
    }

    /// Take all effects accumulated since the last call.
    pub fn drain_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }
}
