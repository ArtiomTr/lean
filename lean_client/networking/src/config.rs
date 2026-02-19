use std::net::IpAddr;

use anyhow::{Result, anyhow};
use libp2p::gossipsub::{
    Config as GossipsubRawConfig, ConfigBuilder, Message, MessageId, ValidationMode,
};
use sha2::{Digest, Sha256};
use std::time::Duration;

use crate::bootnodes::{BootnodeSource, StaticBootnodes};
use crate::types::topics::{GossipTopic, get_topics};

/// 1-byte domain for gossip message-id isolation of valid snappy messages.
pub const MESSAGE_DOMAIN_VALID_SNAPPY: &[u8; 1] = &[0x01];

/// 1-byte domain for gossip message-id isolation of invalid snappy messages.
pub const MESSAGE_DOMAIN_INVALID_SNAPPY: &[u8; 1] = &[0x00];

// ── Chain timing constants ────────────────────────────────────────────────────
// These come from the chain config per leanSpec and are used to calculate
// gossipsub's duplicate cache TTL.

/// Number of slots to look back for justification.
///
/// Per leanSpec: `JUSTIFICATION_LOOKBACK_SLOTS = 3`
pub const JUSTIFICATION_LOOKBACK_SLOTS: u64 = 3;

/// Duration of each slot in seconds.
///
/// Per leanSpec: `SECONDS_PER_SLOT = 4`
pub const SECONDS_PER_SLOT: u64 = 4;

/// Main network configuration.
///
/// Includes both network transport settings and gossipsub configuration,
/// to preserve backward compatibility with callers that pass a single config.
#[derive(Debug, Clone)]
pub struct Config {
    pub socket_address: IpAddr,
    pub socket_port: u16,
    pub discovery_port: u16,
    pub discovery_enabled: bool,
    /// Fork name/digest used to build gossip topic strings (e.g. "devnet0").
    pub fork: String,
    pub(crate) bootnodes: StaticBootnodes,
    /// Embedded gossipsub configuration (populated by `new` or set via `with_gossipsub_config`).
    gossipsub: GossipsubConfig,
}

impl Config {
    pub fn new(
        socket_address: IpAddr,
        socket_port: u16,
        discovery_port: u16,
        discovery_enabled: bool,
        fork: String,
        bootnode_args: Vec<String>,
    ) -> Self {
        let bootnodes = StaticBootnodes::new(
            bootnode_args
                .iter()
                .flat_map(|arg| crate::service::parse_bootnode_arg(arg))
                .collect(),
        );
        let gossipsub = GossipsubConfig::default();
        Config {
            socket_address,
            socket_port,
            discovery_port,
            discovery_enabled,
            fork,
            bootnodes,
            gossipsub,
        }
    }

    /// Legacy constructor that accepts an explicit GossipsubConfig.
    ///
    /// Used by existing callers (e.g. main.rs) that build GossipsubConfig separately.
    pub fn from_gossipsub_config(
        gossipsub_config: GossipsubConfig,
        socket_address: IpAddr,
        socket_port: u16,
        discovery_port: u16,
        discovery_enabled: bool,
        bootnode_args: Vec<String>,
    ) -> Self {
        let bootnodes = StaticBootnodes::new(
            bootnode_args
                .iter()
                .flat_map(|arg| crate::service::parse_bootnode_arg(arg))
                .collect(),
        );
        // Infer fork from topic if possible, otherwise use empty string
        let fork = gossipsub_config
            .topics
            .first()
            .map(|t| t.fork.clone())
            .unwrap_or_default();
        Config {
            socket_address,
            socket_port,
            discovery_port,
            discovery_enabled,
            fork,
            bootnodes,
            gossipsub: gossipsub_config,
        }
    }

    /// Legacy constructor matching the old `NetworkServiceConfig::new` signature.
    ///
    /// New code should prefer `Config::from_gossipsub_config` or `Config::new`.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_gossipsub(
        gossipsub_config: GossipsubConfig,
        socket_address: IpAddr,
        socket_port: u16,
        discovery_port: u16,
        discovery_enabled: bool,
        bootnode_args: Vec<String>,
    ) -> Self {
        Self::from_gossipsub_config(
            gossipsub_config,
            socket_address,
            socket_port,
            discovery_port,
            discovery_enabled,
            bootnode_args,
        )
    }

    pub fn enr_bootnodes(&self) -> Vec<enr::Enr<discv5::enr::CombinedKey>> {
        self.bootnodes.enrs().to_vec()
    }

    pub fn to_multiaddrs(&self) -> Vec<libp2p::Multiaddr> {
        self.bootnodes.to_multiaddrs()
    }

    /// Build gossip topics for this config's fork.
    pub fn gossip_topics(&self) -> Vec<GossipTopic> {
        if !self.gossipsub.topics.is_empty() {
            self.gossipsub.topics.clone()
        } else {
            get_topics(self.fork.clone())
        }
    }

    /// Access the embedded gossipsub configuration.
    pub fn gossipsub_config(&self) -> &GossipsubConfig {
        &self.gossipsub
    }
}

/// Gossipsub configuration parameters.
#[derive(Debug, Clone)]
pub struct GossipsubConfig {
    pub config: GossipsubRawConfig,
    pub topics: Vec<GossipTopic>,
}

impl GossipsubConfig {
    pub fn new() -> Result<Self> {
        // Calculate seen TTL from chain timing constants per leanSpec:
        // seen_ttl = SECONDS_PER_SLOT * JUSTIFICATION_LOOKBACK_SLOTS * 2
        let seen_ttl_secs = SECONDS_PER_SLOT * JUSTIFICATION_LOOKBACK_SLOTS * 2;

        let config = ConfigBuilder::default()
            .heartbeat_interval(Duration::from_millis(700))
            .fanout_ttl(Duration::from_secs(60))
            .history_length(6)
            .history_gossip(3)
            .duplicate_cache_time(Duration::from_secs(seen_ttl_secs))
            .mesh_n(8)
            .mesh_n_low(6)
            .mesh_n_high(12)
            .gossip_lazy(6)
            .validation_mode(ValidationMode::Anonymous)
            .validate_messages()
            .message_id_fn(compute_message_id)
            .build()
            .map_err(|e| anyhow!("Failed to build gossipsub config: {e}"))?;

        Ok(GossipsubConfig {
            config,
            topics: Vec::new(),
        })
    }

    pub fn set_topics(&mut self, topics: Vec<GossipTopic>) {
        self.topics = topics;
    }
}

impl Default for GossipsubConfig {
    fn default() -> Self {
        Self::new().expect("Default GossipsubConfig should always build successfully")
    }
}

/// Computes the gossip message ID per leanSpec:
/// SHA256(domain + uint64_le(len(topic)) + topic + message_data)[:20]
pub fn compute_message_id(message: &Message) -> MessageId {
    let topic_bytes = message.topic.as_str().as_bytes();
    let topic_len = topic_bytes.len() as u64;

    let mut digest_input = Vec::new();
    digest_input.extend_from_slice(MESSAGE_DOMAIN_VALID_SNAPPY);
    digest_input.extend_from_slice(&topic_len.to_le_bytes());
    digest_input.extend_from_slice(topic_bytes);
    digest_input.extend_from_slice(&message.data);

    let hash = Sha256::digest(&digest_input);
    MessageId::from(&hash[..20])
}
