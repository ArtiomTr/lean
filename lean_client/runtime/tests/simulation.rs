use std::{fs, path::PathBuf, sync::Once};

use clock::{Clock, Interval, TestClock, Tick};
use containers::{Block, BlockBody, Slot, State, Validator};
use fork_choice::Store;
use ssz::{SszHash as _, SszWrite as _, H256};
use tracing::level_filters::LevelFilter;

use runtime::{
    simulator::{ChaChaStrategy, NodeSimulator},
    Effect, Event, KeyManager, NetworkEffect, NetworkEvent, ValidatorConfig,
};

fn init_tracing() {
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::builder()
                    .with_default_directive(LevelFilter::INFO.into())
                    .from_env_lossy(),
            )
            .init();
    });
}

fn simulation_assets_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/simulation_assets")
}

fn load_or_generate_validators() -> Vec<Validator> {
    let assets_dir = simulation_assets_dir();
    let mut rand = rand::rng();

    fs::create_dir_all(&assets_dir).unwrap();

    (0..4)
        .map(|index| {
            let sk_path = assets_dir.join(format!("validator_{index}_sk.ssz"));
            let pk_path = assets_dir.join(format!("validator_{index}_pk.txt"));

            let pubkey = match (fs::read(&sk_path), fs::read_to_string(&pk_path)) {
                (Ok(_), Ok(pubkey)) => pubkey.trim().parse().unwrap(),
                _ => {
                    let (pubkey, secret_key) =
                        xmss::SecretKey::generate_key_pair(&mut rand, 0, 2048);

                    fs::write(&sk_path, secret_key.to_ssz().unwrap()).unwrap();
                    fs::write(&pk_path, pubkey.to_string()).unwrap();

                    pubkey
                }
            };

            Validator { pubkey, index }
        })
        .collect()
}

fn prepare_node(validators: Vec<Validator>, validator_index: u64) -> NodeSimulator<ChaChaStrategy> {
    let assets_dir = simulation_assets_dir();

    let anchor_state = State::generate_genesis_with_validators(0, validators);
    let anchor_block = Block {
        slot: Slot(0),
        proposer_index: 0,
        parent_root: H256::zero(),
        state_root: anchor_state.hash_tree_root(),
        body: BlockBody {
            attestations: Default::default(),
        },
    };
    let store = Store::new(anchor_state, anchor_block, Some(0)).unwrap();
    let validator_config = ValidatorConfig {
        validator_indices: vec![validator_index],
    };
    let key_manager = KeyManager::load(&assets_dir, &validator_config.validator_indices).unwrap();

    NodeSimulator::new(
        ChaChaStrategy::with_seed([validator_index as u8; 32]),
        store,
        validator_config,
        key_manager,
    )
}

fn prepare_nodes(validators: &[Validator]) -> Vec<NodeSimulator<ChaChaStrategy>> {
    validators
        .iter()
        .map(|validator| prepare_node(validators.to_vec(), validator.index))
        .collect()
}

fn relay_effects(
    nodes: &mut [NodeSimulator<ChaChaStrategy>],
    source_index: usize,
    effects: &[Effect],
) {
    for effect in effects {
        let event = match effect {
            Effect::Network(NetworkEffect::PublishBlock(block)) => {
                Some(Event::Network(NetworkEvent::GossipBlock(block.clone())))
            }
            Effect::Network(NetworkEffect::PublishAttestation(attestation)) => Some(
                Event::Network(NetworkEvent::GossipAttestation(attestation.clone())),
            ),
            Effect::Network(NetworkEffect::PublishAggregatedAttestation(attestation)) => {
                Some(Event::Network(NetworkEvent::GossipAggregatedAttestation(
                    attestation.clone(),
                )))
            }
            Effect::Network(
                NetworkEffect::RequestBlocksByRoot(_)
                | NetworkEffect::SendStatusRequest { .. }
                | NetworkEffect::SendRequestBlocksByRoot { .. }
                | NetworkEffect::SendResponse { .. }
                | NetworkEffect::DisconnectPeer(_),
            )
            | Effect::Http(_) => None,
        };

        let Some(event) = event else {
            continue;
        };

        for (target_index, node) in nodes.iter_mut().enumerate() {
            if target_index != source_index {
                node.feed(event.clone());
            }
        }
    }
}

fn drain_pending(nodes: &mut [NodeSimulator<ChaChaStrategy>]) {
    while nodes.iter().any(NodeSimulator::has_pending) {
        for node_index in 0..nodes.len() {
            while nodes[node_index].has_pending() {
                let effects = {
                    let node = &mut nodes[node_index];
                    node.step()
                };

                relay_effects(nodes, node_index, &effects);
            }
        }
    }
}

fn assert_nodes_are_consistent(nodes: &[NodeSimulator<ChaChaStrategy>], current_slot: u64) {
    let reference_store = nodes[0].store();
    let reference_head = reference_store.head();
    let reference_justified = *reference_store.latest_justified();
    let reference_finalized = *reference_store.latest_finalized();

    let head_block = reference_store
        .blocks()
        .get(&reference_head)
        .expect("head block must exist");

    assert!(
        head_block.slot.0 <= current_slot,
        "head cannot be ahead of clock"
    );
    assert!(
        reference_finalized.slot <= reference_justified.slot,
        "finalized checkpoint cannot be ahead of justified"
    );
    assert!(
        reference_justified.slot <= head_block.slot,
        "justified checkpoint cannot be ahead of head"
    );
    assert!(
        reference_store.states().contains_key(&reference_head),
        "head state must exist"
    );
    assert!(
        reference_store
            .states()
            .contains_key(&reference_finalized.root),
        "finalized state must exist"
    );

    for node in &nodes[1..] {
        let store = node.store();

        assert_eq!(store.head(), reference_head, "all nodes must share head");
        assert_eq!(
            store.latest_justified(),
            &reference_justified,
            "all nodes must share justified checkpoint"
        );
        assert_eq!(
            store.latest_finalized(),
            &reference_finalized,
            "all nodes must share finalized checkpoint"
        );
        assert!(
            store.blocks().contains_key(&reference_head),
            "all nodes must contain head block"
        );
        assert!(
            store.states().contains_key(&reference_head),
            "all nodes must contain head state"
        );
    }
}

fn feed_tick(node: &mut NodeSimulator<ChaChaStrategy>, clock: &TestClock) {
    node.feed(Event::Tick(Tick {
        slot: clock.current_slot(),
        interval: clock.current_interval(),
    }));
}

#[test]
fn normal_execution() {
    const TICKS: usize = 1000;

    init_tracing();

    let validators = load_or_generate_validators();
    let mut nodes = prepare_nodes(&validators);

    let mut clock = TestClock::new();

    for _ in 0..TICKS {
        drain_pending(&mut nodes);

        if clock.current_interval() == Interval::AttestationAcceptance {
            assert_nodes_are_consistent(&nodes, clock.current_slot());
        }

        clock.tick();

        for node in &mut nodes {
            feed_tick(node, &clock);
        }
    }

    drain_pending(&mut nodes);

    if clock.current_interval() == Interval::AttestationAcceptance {
        assert_nodes_are_consistent(&nodes, clock.current_slot());
    }
}

#[test]
fn skewed_clock_execution() {
    const TICKS: usize = 1000;
    const SKEWED_NODE_INDEX: usize = 0;

    init_tracing();

    let validators = load_or_generate_validators();
    let mut nodes = prepare_nodes(&validators);
    let mut clocks: Vec<_> = (0..nodes.len()).map(|_| TestClock::new()).collect();

    // Start one node a full interval ahead of its peers.
    clocks[SKEWED_NODE_INDEX].tick();

    for (node, clock) in nodes.iter_mut().zip(&clocks) {
        feed_tick(node, clock);
    }

    for _ in 0..TICKS {
        drain_pending(&mut nodes);

        for clock in &mut clocks {
            clock.tick();
        }

        for (node, clock) in nodes.iter_mut().zip(&clocks) {
            feed_tick(node, clock);
        }
    }

    drain_pending(&mut nodes);
}
