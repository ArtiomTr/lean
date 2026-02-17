use anyhow::{Context as _, Result};
use chain::{ChainService, SlotClock};
use clap::Parser;
use containers::{
    Attestation, AttestationData, Block, BlockBody, BlockSignatures, BlockWithAttestation,
    Checkpoint, Config, SignedBlockWithAttestation, Slot, State, Validator,
};
use ethereum_types::H256;
use features::Feature;
use fork_choice::{
    handlers::{on_attestation, on_block},
    store::{Store, get_forkchoice_store, INTERVALS_PER_SLOT},
};
use http_api::HttpServerConfig;
use libp2p_identity::Keypair;
use metrics::{METRICS, Metrics};
use networking::gossipsub::config::GossipsubConfig;
use networking::gossipsub::topic::get_topics;
use networking::network::{NetworkService, NetworkServiceConfig};
use networking::types::{ChainMessage, OutboundP2pRequest};
use ssz::{PersistentList, SszHash};
use std::net::IpAddr;
use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
use tokio::sync::mpsc;
use tokio::task;
use tracing::level_filters::LevelFilter;
use tracing::{error, info, warn};
use validator::{KeyManager, ValidatorConfig, ValidatorService};
use xmss::{PublicKey, Signature};

fn load_node_key(path: &str) -> Result<Keypair, Box<dyn std::error::Error>> {
    let hex_str = std::fs::read_to_string(path)?.trim().to_string();
    let bytes = hex::decode(&hex_str)?;
    let secret = libp2p_identity::secp256k1::SecretKey::try_from_bytes(bytes)?;
    let keypair = libp2p_identity::secp256k1::Keypair::from(secret);
    Ok(Keypair::from(keypair))
}

fn print_chain_status(store: &Store, connected_peers: u64) {
    let current_slot = store.time / INTERVALS_PER_SLOT;

    // Per leanSpec, store.blocks now contains Block (not SignedBlockWithAttestation)
    let head_slot = store.blocks.get(&store.head).map(|b| b.slot.0).unwrap_or(0);

    let behind = if current_slot > head_slot {
        current_slot - head_slot
    } else {
        0
    };

    // Per leanSpec, store.blocks now contains Block (not SignedBlockWithAttestation)
    let (head_root, parent_root, state_root) = if let Some(block) = store.blocks.get(&store.head) {
        let head_root = store.head;
        let parent_root = block.parent_root;
        let state_root = block.state_root;
        (head_root, parent_root, state_root)
    } else {
        (H256::zero(), H256::zero(), H256::zero())
    };

    // Read from store's checkpoints (updated by on_block, reflects highest seen)
    let justified = store.latest_justified.clone();
    let finalized = store.latest_finalized.clone();

    let timely = behind == 0;

    println!("\n+===============================================================+");
    println!(
        "  CHAIN STATUS: Current Slot: {} | Head Slot: {} | Behind: {}",
        current_slot, head_slot, behind
    );
    println!("+---------------------------------------------------------------+");
    println!("  Connected Peers:    {}", connected_peers);
    println!("+---------------------------------------------------------------+");
    println!("  Head Block Root:    0x{:x}", head_root);
    println!("  Parent Block Root:  0x{:x}", parent_root);
    println!("  State Root:         0x{:x}", state_root);
    println!(
        "  Timely:             {}",
        if timely { "YES" } else { "NO" }
    );
    println!("+---------------------------------------------------------------+");
    println!(
        "  Latest Justified:   Slot {:>5} | Root: 0x{:x}",
        justified.slot.0, justified.root
    );
    println!(
        "  Latest Finalized:   Slot {:>5} | Root: 0x{:x}",
        finalized.slot.0, finalized.root
    );
    println!("+===============================================================+\n");
}

#[derive(Parser, Debug)]
struct Args {
    #[arg(short, long, default_value = "127.0.0.1")]
    address: IpAddr,

    #[arg(short, long, default_value_t = 8083)]
    port: u16,

    #[arg(short, long, default_value_t = 8084)]
    discovery_port: u16,

    #[arg(long, default_value_t = false)]
    disable_discovery: bool,

    #[arg(short, long)]
    bootnodes: Vec<String>,

    #[arg(short, long)]
    genesis: Option<String>,

    #[arg(long)]
    node_id: Option<String>,

    /// Path: validators.yaml
    #[arg(long)]
    validator_registry_path: Option<String>,

    /// Path: p2p private key
    #[arg(long)]
    node_key: Option<String>,

    /// Path: directory containing XMSS validator keys (validator_N_sk.ssz files)
    #[arg(long)]
    hash_sig_key_dir: Option<String>,

    #[command(flatten)]
    http_config: HttpServerConfig,

    /// List of optional runtime features to enable
    #[clap(long, value_delimiter = ',')]
    features: Vec<Feature>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    let args = Args::parse();

    for feature in args.features {
        feature.enable();
    }

    let metrics = if args.http_config.metrics_enabled() {
        let metrics = Metrics::new()?;
        metrics.register_with_default_metrics()?;
        let metrics = Arc::new(metrics);
        METRICS.get_or_init(|| metrics.clone());

        Some(metrics)
    } else {
        None
    };

    metrics
        .map(|metrics| {
            metrics.set_client_version("grandine".to_owned(), "0.0.0".to_owned());
            metrics.set_start_time(SystemTime::now())
        })
        .transpose()
        .context("failed to set metrics on start")?;

    let (outbound_p2p_sender, outbound_p2p_receiver) =
        mpsc::unbounded_channel::<OutboundP2pRequest>();
    let (chain_message_sender, mut chain_message_receiver) =
        mpsc::unbounded_channel::<ChainMessage>();

    let (genesis_time, validators) = if let Some(genesis_path) = &args.genesis {
        let genesis_config = containers::GenesisConfig::load_from_file(genesis_path)
            .expect("Failed to load genesis config");

        let validators: Vec<Validator> = genesis_config
            .genesis_validators
            .iter()
            .enumerate()
            .map(|(i, v_str)| {
                let pubkey: PublicKey = v_str.parse().expect("Invalid genesis validator pubkey");
                Validator {
                    pubkey,
                    index: i as u64,
                }
            })
            .collect();

        (genesis_config.genesis_time, validators)
    } else {
        let num_validators = 3;
        let validators = (0..num_validators)
            .map(|i| Validator {
                pubkey: PublicKey::default(),
                index: i as u64,
            })
            .collect();
        (1763757427, validators)
    };

    let genesis_state = State::generate_genesis_with_validators(genesis_time, validators);

    let genesis_block = Block {
        slot: Slot(0),
        proposer_index: 0,
        parent_root: H256::zero(),
        state_root: genesis_state.hash_tree_root(),
        body: BlockBody {
            attestations: Default::default(),
        },
    };

    let genesis_proposer_attestation = Attestation {
        validator_id: 0,
        data: AttestationData {
            slot: Slot(0),
            head: Checkpoint {
                root: H256::zero(),
                slot: Slot(0),
            },
            target: Checkpoint {
                root: H256::zero(),
                slot: Slot(0),
            },
            source: Checkpoint {
                root: H256::zero(),
                slot: Slot(0),
            },
        },
    };
    let genesis_signed_block = SignedBlockWithAttestation {
        message: BlockWithAttestation {
            block: genesis_block,
            proposer_attestation: genesis_proposer_attestation,
        },
        signature: BlockSignatures {
            attestation_signatures: PersistentList::default(),
            proposer_signature: Signature::default(),
        },
    };

    let config = Config { genesis_time };
    let store = get_forkchoice_store(genesis_state.clone(), genesis_signed_block, config);

    let num_validators = genesis_state.validators.len_u64();
    info!(num_validators = num_validators, "Genesis state loaded");

    // Wrap store in Arc<RwLock> for shared access between services
    let store = Arc::new(RwLock::new(store));
    let clock = SlotClock::new(genesis_time);

    // Load validator configuration and keys
    let validator_config_and_keys = if let (Some(node_id), Some(registry_path)) =
        (&args.node_id, &args.validator_registry_path)
    {
        match ValidatorConfig::load_from_file(registry_path, node_id) {
            Ok(config) => {
                let key_manager = if let Some(ref keys_dir) = args.hash_sig_key_dir {
                    let keys_path = std::path::Path::new(keys_dir);
                    if keys_path.exists() {
                        match KeyManager::new(keys_path) {
                            Ok(mut km) => {
                                // Load keys for all assigned validators
                                for &idx in &config.validator_indices {
                                    if let Err(e) = km.load_key(idx) {
                                        warn!(
                                            validator = idx,
                                            error = %e,
                                            "Failed to load key for validator"
                                        );
                                    }
                                }
                                info!(
                                    node_id = %node_id,
                                    indices = ?config.validator_indices,
                                    keys_dir = ?keys_path,
                                    "Validator mode enabled with XMSS signing"
                                );
                                Some(km)
                            }
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    "Failed to load XMSS keys, falling back to zero signatures"
                                );
                                None
                            }
                        }
                    } else {
                        warn!(
                            keys_dir = ?keys_path,
                            "Hash-sig key directory not found, using zero signatures"
                        );
                        None
                    }
                } else {
                    info!(
                        node_id = %node_id,
                        indices = ?config.validator_indices,
                        "Validator mode enabled (no --hash-sig-key-dir - using zero signatures)"
                    );
                    None
                };
                Some((config, key_manager))
            }
            Err(e) => {
                warn!(error = %e, "Failed to load validator config");
                None
            }
        }
    } else {
        info!("Running in passive mode (no validator duties)");
        None
    };

    let fork = "devnet0".to_string();
    let gossipsub_topics = get_topics(fork);
    let mut gossipsub_config = GossipsubConfig::new();
    gossipsub_config.set_topics(gossipsub_topics);

    let discovery_enabled = !args.disable_discovery;

    let network_service_config = Arc::new(NetworkServiceConfig::new(
        gossipsub_config,
        args.address,
        args.port,
        args.discovery_port,
        discovery_enabled,
        args.bootnodes,
    ));

    let peer_count = Arc::new(AtomicU64::new(0));
    let peer_count_for_status = peer_count.clone();

    // LOAD NODE KEY
    let mut network_service = if let Some(key_path) = &args.node_key {
        match load_node_key(key_path) {
            Ok(keypair) => {
                let peer_id = keypair.public().to_peer_id();
                info!(peer_id = %peer_id, "Using custom node key");
                NetworkService::new_with_keypair(
                    network_service_config.clone(),
                    outbound_p2p_receiver,
                    chain_message_sender.clone(),
                    peer_count,
                    keypair,
                )
                .await
                .expect("Failed to create network service with custom key")
            }
            Err(e) => {
                warn!("Failed to load node key: {}, using random key", e);
                NetworkService::new_with_peer_count(
                    network_service_config.clone(),
                    outbound_p2p_receiver,
                    chain_message_sender.clone(),
                    peer_count,
                )
                .await
                .expect("Failed to create network service")
            }
        }
    } else {
        NetworkService::new_with_peer_count(
            network_service_config.clone(),
            outbound_p2p_receiver,
            chain_message_sender.clone(),
            peer_count,
        )
        .await
        .expect("Failed to create network service")
    };

    let network_handle = task::spawn(async move {
        if let Err(err) = network_service.start().await {
            panic!("Network service exited with error: {err}");
        }
    });

    let http_handle = task::spawn(async move {
        if let Err(err) = http_api::run_server(args.http_config).await {
            error!("HTTP Server failed with error: {err:?}");
        }
    });

    // Create ChainService
    let chain_service = ChainService::new(store.clone(), clock.clone());
    let chain_handle = task::spawn(async move {
        if let Err(err) = chain_service.run().await {
            error!("ChainService failed: {err:?}");
        }
    });

    // Create ValidatorService if configured
    let validator_handle = if let Some((config, key_manager)) = validator_config_and_keys {
        let validator_service = ValidatorService::new(
            config,
            num_validators,
            store.clone(),
            clock.clone(),
            outbound_p2p_sender.clone(),
            key_manager,
        );
        Some(task::spawn(async move {
            if let Err(err) = validator_service.run().await {
                error!("ValidatorService failed: {err:?}");
            }
        }))
    } else {
        None
    };

    // Spawn message processing task
    let message_handle = task::spawn(async move {
        let peer_count = peer_count_for_status;

        loop {
            tokio::select! {
                // Print status periodically
                _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
                    let store_read = store.read().expect("Store lock poisoned");
                    let peers = peer_count.load(Ordering::Relaxed);
                    print_chain_status(&store_read, peers);
                }

                // Process incoming network messages
                message = chain_message_receiver.recv() => {
                    let Some(message) = message else { break };
                    match message {
                        ChainMessage::ProcessBlock {
                            signed_block_with_attestation,
                            should_gossip,
                            ..
                        } => {
                            let block_slot = signed_block_with_attestation.message.block.slot;
                            let proposer = signed_block_with_attestation.message.block.proposer_index;
                            let block_root = signed_block_with_attestation.message.block.hash_tree_root();
                            let parent_root = signed_block_with_attestation.message.block.parent_root;

                            info!(
                                slot = block_slot.0,
                                block_root = %format!("0x{:x}", block_root),
                                "Processing block from Validator {}",
                                proposer
                            );

                            let result = {
                                let mut store_write = store.write().expect("Store lock poisoned");
                                on_block(&mut store_write, signed_block_with_attestation.clone())
                            };

                            match result {
                                Ok(()) => {
                                    info!("Block processed successfully");

                                    if should_gossip {
                                        if let Err(e) = outbound_p2p_sender.send(
                                            OutboundP2pRequest::GossipBlockWithAttestation(signed_block_with_attestation)
                                        ) {
                                            warn!("Failed to gossip block: {}", e);
                                        } else {
                                            info!(slot = block_slot.0, "Broadcasted block");
                                        }
                                    }
                                }
                                Err(e) if format!("{e:?}").starts_with("Err: (Fork-choice::Handlers::OnBlock) Block queued") => {
                                    debug!("Block queued, requesting missing parent: {}", e);

                                    // Request missing parent block from peers
                                    if !parent_root.is_zero() {
                                        if let Err(req_err) = outbound_p2p_sender.send(
                                            OutboundP2pRequest::RequestBlocksByRoot(vec![parent_root])
                                        ) {
                                            warn!("Failed to request missing parent block: {}", req_err);
                                        } else {
                                            debug!("Requested missing parent block: 0x{:x}", parent_root);
                                        }
                                    }
                                }
                                Err(e) => warn!("Problem processing block: {}", e),
                            }
                        }
                        ChainMessage::ProcessAttestation {
                            signed_attestation,
                            should_gossip,
                            ..
                        } => {
                            let att_slot = signed_attestation.message.slot.0;
                            let source_slot = signed_attestation.message.source.slot.0;
                            let target_slot = signed_attestation.message.target.slot.0;
                            let validator_id = signed_attestation.validator_id;

                            info!(
                                slot = att_slot,
                                source_slot = source_slot,
                                target_slot = target_slot,
                                "Processing attestation from Validator {}",
                                validator_id
                            );

                            let result = {
                                let mut store_write = store.write().expect("Store lock poisoned");
                                on_attestation(&mut store_write, signed_attestation.clone(), false)
                            };

                            match result {
                                Ok(()) => {
                                    if should_gossip {
                                        if let Err(e) = outbound_p2p_sender.send(
                                            OutboundP2pRequest::GossipAttestation(signed_attestation)
                                        ) {
                                            warn!("Failed to gossip attestation: {}", e);
                                        } else {
                                            info!(slot = att_slot, "Broadcasted attestation");
                                        }
                                    }
                                }
                                Err(e) => warn!("Error processing attestation: {}", e),
                            }
                        }
                    }
                }
            }
        }
    });

    // Wait for any service to finish (which would be an error condition)
    tokio::select! {
        _ = network_handle => {
            info!("Network service finished");
        }
        _ = chain_handle => {
            info!("Chain service finished");
        }
        _ = message_handle => {
            info!("Message processing finished");
        }
        _ = http_handle => {
            info!("HTTP service finished");
        }
        _ = async {
            if let Some(handle) = validator_handle {
                handle.await.ok();
            }
        } => {
            info!("Validator service finished");
        }
    }

    info!("Main async task exiting");

    Ok(())
}
