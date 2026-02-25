use gossipsub::{IdentTopic as Topic, TopicHash};
use serde::{Deserialize, Serialize};
use strum::AsRefStr;
use typenum::Unsigned;
use types::{
    config::Config as ChainConfig,
    nonstandard::Phase,
    phase0::{
        consts::AttestationSubnetCount,
        primitives::{ForkDigest, SubnetId},
    },
};

use crate::Subnet;

pub const TOPIC_PREFIX: &str = "leanconsensus";
pub const SSZ_SNAPPY_ENCODING_POSTFIX: &str = "ssz_snappy";
pub const BLOCK_TOPIC: &str = "blocks";
pub const AGGREGATION_TOPIC: &str = "aggregation";
pub const ATTESTATION_PREFIX: &str = "attestation_";

#[derive(Debug)]
pub struct TopicConfig {
    pub subscribe_all_subnets: bool,
}

pub fn core_topics_to_subscribe(
    _chain_config: &ChainConfig,
    _current_phase: Phase,
    opts: &TopicConfig,
) -> Vec<GossipKind> {
    let mut topics = vec![GossipKind::BeaconBlock, GossipKind::BeaconAggregateAndProof];

    if opts.subscribe_all_subnets {
        for i in 0..AttestationSubnetCount::U64 {
            topics.push(GossipKind::Attestation(i.into()));
        }
    }

    topics
}

pub fn is_fork_non_core_topic(topic: &GossipTopic, _phase: Phase) -> bool {
    match topic.kind() {
        GossipKind::Attestation(_) => true,
        GossipKind::BeaconBlock | GossipKind::BeaconAggregateAndProof => false,
    }
}

pub fn all_topics_at_fork(chain_config: &ChainConfig, current_phase: Phase) -> Vec<GossipKind> {
    let opts = TopicConfig {
        subscribe_all_subnets: true,
    };
    core_topics_to_subscribe(chain_config, current_phase, &opts)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct GossipTopic {
    /// The encoding of the topic.
    encoding: GossipEncoding,
    /// The fork digest that identifies the network/fork.
    pub fork_digest: ForkDigest,
    /// The kind of topic.
    kind: GossipKind,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, AsRefStr)]
#[strum(serialize_all = "snake_case")]
pub enum GossipKind {
    BeaconBlock,
    BeaconAggregateAndProof,
    #[strum(serialize = "attestation")]
    Attestation(SubnetId),
}

impl std::fmt::Display for GossipKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GossipKind::Attestation(subnet_id) => write!(f, "attestation_{}", *subnet_id),
            GossipKind::BeaconBlock => f.write_str(BLOCK_TOPIC),
            GossipKind::BeaconAggregateAndProof => f.write_str(AGGREGATION_TOPIC),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
pub enum GossipEncoding {
    #[default]
    SSZSnappy,
}

impl GossipTopic {
    pub fn new(kind: GossipKind, encoding: GossipEncoding, fork_digest: ForkDigest) -> Self {
        GossipTopic {
            encoding,
            fork_digest,
            kind,
        }
    }

    pub fn encoding(&self) -> &GossipEncoding {
        &self.encoding
    }

    pub fn kind(&self) -> &GossipKind {
        &self.kind
    }

    pub fn decode(topic: &str) -> Result<Self, String> {
        // Format: /{prefix}/{fork_digest_hex}/{topic_name}/{encoding}
        let topic_parts: Vec<&str> = topic.split('/').collect();
        if topic_parts.len() == 5 && topic_parts[1] == TOPIC_PREFIX {
            let encoding = match topic_parts[4] {
                SSZ_SNAPPY_ENCODING_POSTFIX => GossipEncoding::SSZSnappy,
                _ => return Err(format!("Unknown encoding: {}", topic)),
            };
            let fork_digest_hex = topic_parts[2];
            let fork_digest_bytes = hex::decode(fork_digest_hex)
                .map_err(|_| format!("Invalid fork digest hex: {}", topic))?;
            if fork_digest_bytes.len() != 4 {
                return Err(format!("Fork digest must be 4 bytes: {}", topic));
            }
            let mut fork_digest = [0u8; 4];
            fork_digest.copy_from_slice(&fork_digest_bytes);

            let kind = match topic_parts[3] {
                BLOCK_TOPIC => GossipKind::BeaconBlock,
                AGGREGATION_TOPIC => GossipKind::BeaconAggregateAndProof,
                topic => match subnet_topic_index(topic) {
                    Some(kind) => kind,
                    None => return Err(format!("Unknown topic: {}", topic)),
                },
            };

            return Ok(GossipTopic {
                encoding,
                fork_digest,
                kind,
            });
        }

        Err(format!("Unknown topic: {}", topic))
    }

    pub fn subnet_id(&self) -> Option<Subnet> {
        match self.kind() {
            GossipKind::Attestation(subnet_id) => Some(Subnet::Attestation(*subnet_id)),
            _ => None,
        }
    }
}

impl From<GossipTopic> for Topic {
    fn from(topic: GossipTopic) -> Topic {
        Topic::new(topic)
    }
}

impl From<GossipTopic> for String {
    fn from(topic: GossipTopic) -> String {
        topic.to_string()
    }
}

impl std::fmt::Display for GossipTopic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let encoding = match self.encoding {
            GossipEncoding::SSZSnappy => SSZ_SNAPPY_ENCODING_POSTFIX,
        };
        let kind_str = self.kind.to_string();
        let fork_digest_hex = hex::encode(self.fork_digest);
        write!(
            f,
            "/{}/{}/{}/{}",
            TOPIC_PREFIX, fork_digest_hex, kind_str, encoding
        )
    }
}

impl From<Subnet> for GossipKind {
    fn from(subnet_id: Subnet) -> Self {
        match subnet_id {
            Subnet::Attestation(s) => GossipKind::Attestation(s),
        }
    }
}

pub fn subnet_from_topic_hash(topic_hash: &TopicHash) -> Option<Subnet> {
    GossipTopic::decode(topic_hash.as_str()).ok()?.subnet_id()
}

fn subnet_topic_index(topic: &str) -> Option<GossipKind> {
    if let Some(index) = topic.strip_prefix(ATTESTATION_PREFIX) {
        return Some(GossipKind::Attestation(index.parse::<SubnetId>().ok()?));
    }
    None
}
