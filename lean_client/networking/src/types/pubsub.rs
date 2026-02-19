use anyhow::{Context as _, Result};
use containers::{SignedAttestation, SignedBlockWithAttestation};
use libp2p::gossipsub::{DataTransform, Message, RawMessage, TopicHash};
use snap::raw::{Decoder, Encoder};
use ssz::SszReadDefault as _;

use super::topics::{GossipKind, GossipTopic};

/// All gossip message types for lean ethereum.
///
/// Following eth2_libp2p's `PubsubMessage` pattern.
pub enum PubsubMessage {
    Block(SignedBlockWithAttestation),
    Attestation(SignedAttestation),
}

impl PubsubMessage {
    /// Decode a raw gossipsub message into a typed `PubsubMessage`.
    pub fn decode(topic: &TopicHash, data: &[u8]) -> Result<Self> {
        match GossipTopic::decode(topic)?.kind {
            GossipKind::Block => Ok(Self::Block(
                SignedBlockWithAttestation::from_ssz_default(data)
                    .context("SSZ decode SignedBlockWithAttestation failed")?,
            )),
            GossipKind::Attestation => Ok(Self::Attestation(
                SignedAttestation::from_ssz_default(data)
                    .context("SSZ decode SignedAttestation failed")?,
            )),
        }
    }
}

/// Snappy DataTransform for gossipsub message compression.
///
/// Following eth2_libp2p's `SnappyTransform` pattern. Compresses outbound
/// messages and decompresses inbound messages using snappy raw format.
pub struct SnappyTransform;

impl Default for SnappyTransform {
    fn default() -> Self {
        Self
    }
}

impl DataTransform for SnappyTransform {
    fn inbound_transform(&self, raw_message: RawMessage) -> Result<Message, std::io::Error> {
        let mut decoder = Decoder::new();
        let data = decoder.decompress_vec(&raw_message.data)?;
        Ok(Message {
            topic: raw_message.topic,
            data,
            sequence_number: raw_message.sequence_number,
            source: raw_message.source,
        })
    }

    fn outbound_transform(
        &self,
        _topic: &TopicHash,
        data: Vec<u8>,
    ) -> Result<Vec<u8>, std::io::Error> {
        let mut encoder = Encoder::new();
        encoder
            .compress_vec(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_decode_invalid_topic() {
        let topic = TopicHash::from_raw("/invalid/topic/format");
        let result = PubsubMessage::decode(&topic, b"some_data");
        assert!(result.is_err());
    }

    #[test]
    fn test_message_decode_invalid_ssz_for_block() {
        use crate::types::topics::{BLOCK_TOPIC, SSZ_SNAPPY_ENCODING_POSTFIX, TOPIC_PREFIX};
        let topic_str = format!(
            "/{}/{}/{}/{}",
            TOPIC_PREFIX, "genesis", BLOCK_TOPIC, SSZ_SNAPPY_ENCODING_POSTFIX
        );
        let topic = TopicHash::from_raw(topic_str);
        let result = PubsubMessage::decode(&topic, b"not_valid_ssz");
        assert!(result.is_err());
    }

    #[test]
    fn test_message_decode_invalid_ssz_for_attestation() {
        use crate::types::topics::{ATTESTATION_TOPIC, SSZ_SNAPPY_ENCODING_POSTFIX, TOPIC_PREFIX};
        let topic_str = format!(
            "/{}/{}/{}/{}",
            TOPIC_PREFIX, "genesis", ATTESTATION_TOPIC, SSZ_SNAPPY_ENCODING_POSTFIX
        );
        let topic = TopicHash::from_raw(topic_str);
        let result = PubsubMessage::decode(&topic, b"not_valid_ssz");
        assert!(result.is_err());
    }

    #[test]
    fn test_message_decode_empty_data_fails() {
        use crate::types::topics::{BLOCK_TOPIC, SSZ_SNAPPY_ENCODING_POSTFIX, TOPIC_PREFIX};
        let topic_str = format!(
            "/{}/{}/{}/{}",
            TOPIC_PREFIX, "genesis", BLOCK_TOPIC, SSZ_SNAPPY_ENCODING_POSTFIX
        );
        let topic = TopicHash::from_raw(topic_str);
        let result = PubsubMessage::decode(&topic, &[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_message_decode_wrong_prefix() {
        let topic = TopicHash::from_raw("/eth2/genesis/block/ssz_snappy");
        let result = PubsubMessage::decode(&topic, b"some_data");
        assert!(result.is_err());
    }

    #[test]
    fn test_message_decode_wrong_encoding() {
        use crate::types::topics::{BLOCK_TOPIC, TOPIC_PREFIX};
        let topic_str = format!("/{}/{}/{}/json", TOPIC_PREFIX, "genesis", BLOCK_TOPIC);
        let topic = TopicHash::from_raw(topic_str);
        let result = PubsubMessage::decode(&topic, b"some_data");
        assert!(result.is_err());
    }

    #[test]
    fn test_message_decode_unsupported_kind() {
        use crate::types::topics::{SSZ_SNAPPY_ENCODING_POSTFIX, TOPIC_PREFIX};
        let topic_str = format!(
            "/{}/{}/{}/{}",
            TOPIC_PREFIX, "genesis", "voluntary_exit", SSZ_SNAPPY_ENCODING_POSTFIX
        );
        let topic = TopicHash::from_raw(topic_str);
        let result = PubsubMessage::decode(&topic, b"some_data");
        assert!(result.is_err());
    }
}
