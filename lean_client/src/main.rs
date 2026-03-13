use anyhow::{Context as _, Result};
use clap::Parser;
use containers::{Block, BlockBody, Slot, State, Validator};
use ethereum_types::H256;
use features::Feature;
use fork_choice::Store;
use http_api::HttpServerConfig;
use networking::{Multiaddr, NetworkConfig as NetworkServiceConfig};
use runtime::{KeyManager, Node, ValidatorConfig};
use ssz::SszHash;
use std::net::IpAddr;
use std::path::PathBuf;
use tracing::level_filters::LevelFilter;
use tracing::{info, warn};
use validator::ValidatorConfig as RegistryValidatorConfig;
use xmss::PublicKey;

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

    for feature in &args.features {
        feature.enable();
    }

    // ── Genesis ───────────────────────────────────────────────────────────────

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
        (1763757427u64, validators)
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

    let store = Store::new(genesis_state.clone(), genesis_block, None)
        .context("Failed to initialize forkchoice store")?;

    info!(
        num_validators = genesis_state.validators.len_u64(),
        "Genesis state loaded"
    );

    // ── Validator configuration ───────────────────────────────────────────────
    //
    // Load the validator registry to discover which indices this node controls,
    // then build the runtime `ValidatorConfig` and `KeyManager` from those indices.

    let (validator_config, key_manager): (Option<ValidatorConfig>, Option<KeyManager>) = if let (
        Some(node_id),
        Some(registry_path),
    ) =
        (&args.node_id, &args.validator_registry_path)
    {
        match RegistryValidatorConfig::load_from_file(registry_path, node_id) {
            Ok(registry_config) => {
                let indices = registry_config.validator_indices.clone();

                let km = if let Some(ref keys_dir) = args.hash_sig_key_dir {
                    match KeyManager::load(keys_dir, &indices) {
                        Ok(km) => {
                            info!(
                                node_id = %node_id,
                                ?indices,
                                keys_dir,
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
                    info!(
                        node_id = %node_id,
                        ?indices,
                        "Validator mode enabled (no --hash-sig-key-dir — using zero signatures)"
                    );
                    None
                };

                let vc = ValidatorConfig {
                    validator_indices: indices,
                };
                (Some(vc), km)
            }
            Err(e) => {
                warn!(error = %e, "Failed to load validator config");
                (None, None)
            }
        }
    } else {
        info!("Running in passive mode (no validator duties)");
        (None, None)
    };

    // ── Network configuration ─────────────────────────────────────────────────

    let mut network_config = NetworkServiceConfig::default();
    network_config.disable_discovery = args.disable_discovery;

    match args.address {
        IpAddr::V4(ipv4) => network_config.set_ipv4_listening_address(
            ipv4,
            args.port,
            args.discovery_port,
            args.port.saturating_add(1),
        ),
        IpAddr::V6(ipv6) => network_config.set_ipv6_listening_address(
            ipv6,
            args.port,
            args.discovery_port,
            args.port.saturating_add(1),
        ),
    }

    let mut parsed_bootnodes = Vec::new();
    for bootnode in &args.bootnodes {
        match bootnode.parse::<Multiaddr>() {
            Ok(addr) => parsed_bootnodes.push(addr),
            Err(error) => warn!(%bootnode, %error, "Skipping invalid bootnode multiaddr"),
        }
    }
    network_config.boot_nodes_multiaddr = parsed_bootnodes.clone();
    network_config.libp2p_nodes = parsed_bootnodes;

    if let Some(key_path) = &args.node_key {
        network_config.libp2p_private_key_file = Some(PathBuf::from(key_path));
        info!(%key_path, "Using custom node key path");
    }

    // ── Node ──────────────────────────────────────────────────────────────────

    let node = Node::new(
        genesis_time,
        store,
        validator_config,
        key_manager,
        network_config,
        args.http_config,
    )
    .context("Failed to create node")?;

    // The node builds its own tokio runtime internally; run it in a
    // blocking thread so it doesn't fight with the outer async runtime.
    let node_handle = tokio::task::spawn_blocking(move || node.run());

    match node_handle.await {
        Ok(Ok(())) => info!("Node finished"),
        Ok(Err(e)) => tracing::error!("Node error: {e:?}"),
        Err(e) => tracing::error!("Node panicked: {e:?}"),
    }

    info!("Main task exiting");
    Ok(())
}
