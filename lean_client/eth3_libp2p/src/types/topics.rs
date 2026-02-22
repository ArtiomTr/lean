use gossipsub::{IdentTopic as Topic, TopicHash};
use serde::{Deserialize, Serialize};
use strum::AsRefStr;
use typenum::Unsigned;
use types::{
    config::Config as ChainConfig,
    nonstandard::Phase,
    phase0::{consts::AttestationSubnetCount, primitives::SubnetId},
};

use crate::Subnet;

/// The gossipsub topic names.
// These constants form a topic name of the form /TOPIC_PREFIX/TOPIC/ENCODING_POSTFIX
// For example /leanconsensus/beacon_block/ssz_snappy
pub const TOPIC_PREFIX: &str = "leanconsensus";
pub const SSZ_SNAPPY_ENCODING_POSTFIX: &str = "ssz_snappy";
pub const BEACON_BLOCK_TOPIC: &str = "beacon_block";
pub const BEACON_AGGREGATE_AND_PROOF_TOPIC: &str = "beacon_aggregate_and_proof";
pub const BEACON_ATTESTATION_PREFIX: &str = "beacon_attestation_";

#[derive(Debug)]
pub struct TopicConfig {
    pub subscribe_all_subnets: bool,
}

/// Returns all the topics the node should subscribe at `current_phase`
pub fn core_topics_to_subscribe(
    _chain_config: &ChainConfig,
    _current_phase: Phase,
    opts: &TopicConfig,
) -> Vec<GossipKind> {
    let mut topics = vec![
        GossipKind::BeaconBlock,
        GossipKind::BeaconAggregateAndProof,
    ];

    if opts.subscribe_all_subnets {
        for i in 0..AttestationSubnetCount::U64 {
            topics.push(GossipKind::Attestation(i.into()));
        }
    }

    topics
}

/// Returns true if a given non-core `GossipTopic` MAY be subscribe at this fork.
///
/// For example: the `Attestation` topic is not subscribed as a core topic if
/// subscribe_all_subnets = false` but we may subscribe to it outside of a fork
/// boundary if the node is an aggregator.
pub fn is_fork_non_core_topic(topic: &GossipTopic, _phase: Phase) -> bool {
    match topic.kind() {
        // Node may be aggregator of attestation topics
        GossipKind::Attestation(_) => true,
        // All these topics are core-only
        GossipKind::BeaconBlock | GossipKind::BeaconAggregateAndProof => false,
    }
}

pub fn all_topics_at_fork(chain_config: &ChainConfig, current_phase: Phase) -> Vec<GossipKind> {
    let opts = TopicConfig {
        subscribe_all_subnets: true,
    };
    core_topics_to_subscribe(chain_config, current_phase, &opts)
}

/// A gossipsub topic which encapsulates the type of messages that should be sent and received over
/// the pubsub protocol and the way the messages should be encoded.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct GossipTopic {
    /// The encoding of the topic.
    encoding: GossipEncoding,
    /// The kind of topic.
    kind: GossipKind,
}

/// Enum that brings these topics into the rust type system.
// NOTE: There is intentionally no unknown type here. We only allow known gossipsub topics.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, AsRefStr)]
#[strum(serialize_all = "snake_case")]
pub enum GossipKind {
    /// Topic for publishing beacon blocks.
    BeaconBlock,
    /// Topic for publishing aggregate attestations and proofs.
    BeaconAggregateAndProof,
    /// Topic for publishing raw attestations on a particular subnet.
    #[strum(serialize = "beacon_attestation")]
    Attestation(SubnetId),
}

impl std::fmt::Display for GossipKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GossipKind::Attestation(subnet_id) => write!(f, "beacon_attestation_{}", *subnet_id),
            x => f.write_str(x.as_ref()),
        }
    }
}

/// The known encoding types for gossipsub messages.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
pub enum GossipEncoding {
    /// Messages are encoded with SSZSnappy.
    #[default]
    SSZSnappy,
}

impl GossipTopic {
    pub fn new(kind: GossipKind, encoding: GossipEncoding) -> Self {
        GossipTopic { encoding, kind }
    }

    /// Returns the encoding type for the gossipsub topic.
    pub fn encoding(&self) -> &GossipEncoding {
        &self.encoding
    }

    /// Returns the kind of message expected on the gossipsub topic.
    pub fn kind(&self) -> &GossipKind {
        &self.kind
    }

    pub fn decode(topic: &str) -> Result<Self, String> {
        let topic_parts: Vec<&str> = topic.split('/').collect();
        if topic_parts.len() == 4 && topic_parts[1] == TOPIC_PREFIX {
            let encoding = match topic_parts[3] {
                SSZ_SNAPPY_ENCODING_POSTFIX => GossipEncoding::SSZSnappy,
                _ => return Err(format!("Unknown encoding: {}", topic)),
            };
            let kind = match topic_parts[2] {
                BEACON_BLOCK_TOPIC => GossipKind::BeaconBlock,
                BEACON_AGGREGATE_AND_PROOF_TOPIC => GossipKind::BeaconAggregateAndProof,
                topic => match subnet_topic_index(topic) {
                    Some(kind) => kind,
                    None => return Err(format!("Unknown topic: {}", topic)),
                },
            };

            return Ok(GossipTopic { encoding, kind });
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
        // Use the `Display` implementation below.
        topic.to_string()
    }
}

impl std::fmt::Display for GossipTopic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let encoding = match self.encoding {
            GossipEncoding::SSZSnappy => SSZ_SNAPPY_ENCODING_POSTFIX,
        };

        let kind = match self.kind {
            GossipKind::BeaconBlock => BEACON_BLOCK_TOPIC.into(),
            GossipKind::BeaconAggregateAndProof => BEACON_AGGREGATE_AND_PROOF_TOPIC.into(),
            GossipKind::Attestation(index) => format!("{}{}", BEACON_ATTESTATION_PREFIX, index),
        };
        write!(f, "/{}/{}/{}", TOPIC_PREFIX, kind, encoding)
    }
}

impl From<Subnet> for GossipKind {
    fn from(subnet_id: Subnet) -> Self {
        match subnet_id {
            Subnet::Attestation(s) => GossipKind::Attestation(s),
        }
    }
}

// helper functions

/// Get subnet id from an attestation subnet topic hash.
pub fn subnet_from_topic_hash(topic_hash: &TopicHash) -> Option<Subnet> {
    GossipTopic::decode(topic_hash.as_str()).ok()?.subnet_id()
}

// Determines if the topic name is of an indexed topic.
fn subnet_topic_index(topic: &str) -> Option<GossipKind> {
    if let Some(index) = topic.strip_prefix(BEACON_ATTESTATION_PREFIX) {
        return Some(GossipKind::Attestation(index.parse::<SubnetId>().ok()?));
    }
    None
}

#[cfg(test)]
mod tests {
    use enum_iterator::Sequence;

    use super::GossipKind::*;
    use super::*;

    const BAD_PREFIX: &str = "tezos";
    const BAD_ENCODING: &str = "rlp";
    const BAD_KIND: &str = "blocks";

    fn topics() -> Vec<String> {
        let mut topics = Vec::new();
        for encoding in [GossipEncoding::SSZSnappy].iter() {
            for kind in [BeaconBlock, BeaconAggregateAndProof, Attestation(42)].iter() {
                topics.push(GossipTopic::new(kind.clone(), encoding.clone()).into());
            }
        }
        topics
    }

    fn create_topic(prefix: &str, kind: &str, encoding: &str) -> String {
        format!("/{}/{}/{}", prefix, kind, encoding)
    }

    #[test]
    fn test_decode() {
        for topic in topics().iter() {
            assert!(GossipTopic::decode(topic.as_str()).is_ok());
        }
    }

    #[test]
    fn test_decode_malicious() {
        let bad_prefix_str = create_topic(BAD_PREFIX, BEACON_BLOCK_TOPIC, SSZ_SNAPPY_ENCODING_POSTFIX);
        assert!(GossipTopic::decode(bad_prefix_str.as_str()).is_err());

        let bad_kind_str = create_topic(TOPIC_PREFIX, BAD_KIND, SSZ_SNAPPY_ENCODING_POSTFIX);
        assert!(GossipTopic::decode(bad_kind_str.as_str()).is_err());

        let bad_encoding_str = create_topic(TOPIC_PREFIX, BEACON_BLOCK_TOPIC, BAD_ENCODING);
        assert!(GossipTopic::decode(bad_encoding_str.as_str()).is_err());

        // Extra parts
        assert!(
            GossipTopic::decode("/leanconsensus/beacon_block/ssz_snappy/yolo").is_err(),
            "should have exactly 4 parts"
        );
        // Empty string
        assert!(GossipTopic::decode("").is_err());
        // Empty parts
        assert!(GossipTopic::decode("////").is_err());
    }

    #[test]
    fn test_subnet_from_topic_hash() {
        let topic_hash = TopicHash::from_raw("/leanconsensus/beacon_block/ssz_snappy");
        assert!(subnet_from_topic_hash(&topic_hash).is_none());

        let topic_hash =
            TopicHash::from_raw("/leanconsensus/beacon_attestation_42/ssz_snappy");
        assert_eq!(
            subnet_from_topic_hash(&topic_hash),
            Some(Subnet::Attestation(42))
        );
    }

    #[test]
    fn test_as_str_ref() {
        assert_eq!("beacon_block", BeaconBlock.as_ref());
        assert_eq!(
            "beacon_aggregate_and_proof",
            BeaconAggregateAndProof.as_ref()
        );
        assert_eq!("beacon_attestation", Attestation(42).as_ref());

    }

    fn get_chain_config() -> ChainConfig {
        let mut config = ChainConfig::default();
        config.altair_fork_epoch = 1;
        config.bellatrix_fork_epoch = 2;
        config.capella_fork_epoch = 3;
        config.deneb_fork_epoch = 4;
        config.electra_fork_epoch = 5;
        config.fulu_fork_epoch = 6;
        config
    }

    fn get_topic_config() -> TopicConfig {
        TopicConfig {
            subscribe_all_subnets: false,
        }
    }

    #[test]
    fn base_topics_are_always_active() {
        let config = get_chain_config();
        let topic_config = get_topic_config();
        for phase in enum_iterator::all() {
            assert!(
                core_topics_to_subscribe(&config, phase, &topic_config)
                    .contains(&GossipKind::BeaconBlock)
            );
        }
    }

    #[test]
    fn test_core_topics_to_subscribe() {
        let config = get_chain_config();
        let topic_config = get_topic_config();
        let latest_fork = Phase::last().unwrap_or(Phase::Phase0);
        let topics = core_topics_to_subscribe(&config, latest_fork, &topic_config);

        let expected_topics = vec![
            GossipKind::BeaconBlock,
            GossipKind::BeaconAggregateAndProof,
        ];
        // Need to check all the topics exist in an order independent manner
        for expected_topic in expected_topics {
            assert!(
                topics.contains(&expected_topic),
                "Should contain {:?}",
                expected_topic,
            );
        }
    }
}
