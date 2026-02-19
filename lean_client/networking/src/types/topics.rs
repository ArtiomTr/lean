use anyhow::{Result, anyhow};
use libp2p::gossipsub::{IdentTopic, TopicHash};

pub const TOPIC_PREFIX: &str = "leanconsensus";
pub const SSZ_SNAPPY_ENCODING_POSTFIX: &str = "ssz_snappy";
pub const BLOCK_TOPIC: &str = "block";
pub const ATTESTATION_TOPIC: &str = "attestation";

/// Gossip topic combining fork digest, kind, and encoding.
///
/// Mirrors the `GossipTopic` pattern from eth2_libp2p, adapted for lean ethereum's
/// simpler topic set (block + attestation only).
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct GossipTopic {
    pub fork: String,
    pub kind: GossipKind,
}

/// Gossip message kind (topic type).
#[derive(Debug, Hash, Clone, Copy, PartialEq, Eq)]
pub enum GossipKind {
    Block,
    Attestation,
}

/// Build all gossip topics for the given fork.
pub fn get_topics(fork: String) -> Vec<GossipTopic> {
    vec![
        GossipTopic {
            fork: fork.clone(),
            kind: GossipKind::Block,
        },
        GossipTopic {
            fork: fork.clone(),
            kind: GossipKind::Attestation,
        },
    ]
}

impl GossipTopic {
    pub fn decode(topic: &TopicHash) -> Result<Self> {
        let parts = Self::split_topic(topic)?;
        Self::validate_parts(&parts, topic)?;
        let fork = parts[1].to_string();
        let kind = Self::extract_kind(parts[2])?;
        Ok(GossipTopic { fork, kind })
    }

    fn split_topic(topic: &TopicHash) -> Result<Vec<&str>> {
        let parts: Vec<&str> = topic.as_str().trim_start_matches('/').split('/').collect();
        if parts.len() != 4 {
            return Err(anyhow!("Invalid topic part count: {topic:?}"));
        }
        Ok(parts)
    }

    fn validate_parts(parts: &[&str], topic: &TopicHash) -> Result<()> {
        if parts[0] != TOPIC_PREFIX || parts[3] != SSZ_SNAPPY_ENCODING_POSTFIX {
            return Err(anyhow!("Invalid topic parts: {topic:?}"));
        }
        Ok(())
    }

    fn extract_kind(kind_str: &str) -> Result<GossipKind> {
        match kind_str {
            BLOCK_TOPIC => Ok(GossipKind::Block),
            ATTESTATION_TOPIC => Ok(GossipKind::Attestation),
            other => Err(anyhow!("Invalid topic kind: {other:?}")),
        }
    }
}

impl std::fmt::Display for GossipTopic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "/{}/{}/{}/{}",
            TOPIC_PREFIX, self.fork, self.kind, SSZ_SNAPPY_ENCODING_POSTFIX
        )
    }
}

impl std::fmt::Display for GossipKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GossipKind::Block => write!(f, "{BLOCK_TOPIC}"),
            GossipKind::Attestation => write!(f, "{ATTESTATION_TOPIC}"),
        }
    }
}

impl From<GossipTopic> for IdentTopic {
    fn from(topic: GossipTopic) -> IdentTopic {
        IdentTopic::new(topic)
    }
}

impl From<GossipTopic> for String {
    fn from(topic: GossipTopic) -> Self {
        topic.to_string()
    }
}

impl From<GossipTopic> for TopicHash {
    fn from(val: GossipTopic) -> Self {
        let kind_str = match &val.kind {
            GossipKind::Block => BLOCK_TOPIC,
            GossipKind::Attestation => ATTESTATION_TOPIC,
        };
        TopicHash::from_raw(format!(
            "/{}/{}/{}/{}",
            TOPIC_PREFIX, val.fork, kind_str, SSZ_SNAPPY_ENCODING_POSTFIX
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_topic_decode_valid_block() {
        let topic_str = format!(
            "/{}/{}/{}/{}",
            TOPIC_PREFIX, "genesis", BLOCK_TOPIC, SSZ_SNAPPY_ENCODING_POSTFIX
        );
        let topic_hash = TopicHash::from_raw(topic_str);
        let decoded = GossipTopic::decode(&topic_hash).unwrap();
        assert_eq!(decoded.fork, "genesis");
        assert_eq!(decoded.kind, GossipKind::Block);
    }

    #[test]
    fn test_topic_decode_valid_attestation() {
        let topic_str = format!(
            "/{}/{}/{}/{}",
            TOPIC_PREFIX, "genesis", ATTESTATION_TOPIC, SSZ_SNAPPY_ENCODING_POSTFIX
        );
        let topic_hash = TopicHash::from_raw(topic_str);
        let decoded = GossipTopic::decode(&topic_hash).unwrap();
        assert_eq!(decoded.fork, "genesis");
        assert_eq!(decoded.kind, GossipKind::Attestation);
    }

    #[test]
    fn test_topic_decode_invalid_prefix() {
        let topic_str = format!(
            "/{}/{}/{}/{}",
            "wrongprefix", "genesis", BLOCK_TOPIC, SSZ_SNAPPY_ENCODING_POSTFIX
        );
        let topic_hash = TopicHash::from_raw(topic_str);
        assert!(GossipTopic::decode(&topic_hash).is_err());
    }

    #[test]
    fn test_topic_decode_invalid_encoding() {
        let topic_str = format!(
            "/{}/{}/{}/{}",
            TOPIC_PREFIX, "genesis", BLOCK_TOPIC, "wrong_encoding"
        );
        let topic_hash = TopicHash::from_raw(topic_str);
        assert!(GossipTopic::decode(&topic_hash).is_err());
    }

    #[test]
    fn test_topic_decode_invalid_kind() {
        let topic_str = format!(
            "/{}/{}/{}/{}",
            TOPIC_PREFIX, "genesis", "invalid_kind", SSZ_SNAPPY_ENCODING_POSTFIX
        );
        let topic_hash = TopicHash::from_raw(topic_str);
        assert!(GossipTopic::decode(&topic_hash).is_err());
    }

    #[test]
    fn test_topic_decode_invalid_part_count() {
        let topic_hash = TopicHash::from_raw("/only/two/parts");
        assert!(GossipTopic::decode(&topic_hash).is_err());
    }

    #[test]
    fn test_topic_to_string() {
        let topic = GossipTopic {
            fork: "genesis".to_string(),
            kind: GossipKind::Block,
        };
        let topic_str = topic.to_string();
        assert_eq!(
            topic_str,
            format!(
                "/{}/{}/{}/{}",
                TOPIC_PREFIX, "genesis", BLOCK_TOPIC, SSZ_SNAPPY_ENCODING_POSTFIX
            )
        );
    }

    #[test]
    fn test_topic_encoding_decoding_roundtrip() {
        let original = GossipTopic {
            fork: "testfork".to_string(),
            kind: GossipKind::Attestation,
        };
        let topic_hash: TopicHash = original.clone().into();
        let decoded = GossipTopic::decode(&topic_hash).unwrap();
        assert_eq!(original.fork, decoded.fork);
        assert_eq!(original.kind, decoded.kind);
    }

    #[test]
    fn test_get_topics_all_same_fork() {
        let topics = get_topics("myfork".to_string());
        assert_eq!(topics.len(), 2);
        let kinds: Vec<_> = topics.iter().map(|t| t.kind).collect();
        assert!(kinds.contains(&GossipKind::Block));
        assert!(kinds.contains(&GossipKind::Attestation));
        for topic in &topics {
            assert_eq!(topic.fork, "myfork");
        }
    }

    #[test]
    fn test_gossip_kind_display() {
        assert_eq!(GossipKind::Block.to_string(), BLOCK_TOPIC);
        assert_eq!(GossipKind::Attestation.to_string(), ATTESTATION_TOPIC);
    }

    #[test]
    fn test_topic_equality() {
        let topic1 = GossipTopic {
            fork: "genesis".to_string(),
            kind: GossipKind::Block,
        };
        let topic2 = GossipTopic {
            fork: "genesis".to_string(),
            kind: GossipKind::Block,
        };
        let topic3 = GossipTopic {
            fork: "genesis".to_string(),
            kind: GossipKind::Attestation,
        };
        assert_eq!(topic1, topic2);
        assert_ne!(topic1, topic3);
    }

    #[test]
    fn test_topic_hash_conversion() {
        let topic = GossipTopic {
            fork: "genesis".to_string(),
            kind: GossipKind::Block,
        };
        let hash: TopicHash = topic.into();
        let expected = format!(
            "/{}/{}/{}/{}",
            TOPIC_PREFIX, "genesis", BLOCK_TOPIC, SSZ_SNAPPY_ENCODING_POSTFIX
        );
        assert_eq!(hash.as_str(), expected);
    }
}
