//! Handles the encoding and decoding of pubsub messages.

use crate::TopicHash;
use crate::types::{GossipEncoding, GossipKind, GossipTopic};
use containers::{
    ForkDigest, SignedAggregatedAttestation, SignedAttestation, SignedBlockWithAttestation,
    SubnetId,
};
use libp2p::gossipsub;
use snap::raw::{Decoder, Encoder, decompress_len};
use ssz::{SszReadDefault, SszWrite as _, WriteError};
use std::boxed::Box;
use std::fmt;
use std::io::{Error, ErrorKind};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub enum PubsubMessage {
    /// Gossipsub message providing notification of a new block.
    Block(Arc<SignedBlockWithAttestation>),
    /// Gossipsub message providing notification of a raw un-aggregated attestation with its shard id.
    Attestation(SubnetId, Arc<SignedAttestation>),
    /// Gosspisub message providing notification of a Aggregate attestation and associated proof.
    AggregatedAttestation(Arc<SignedAggregatedAttestation>),
}

// Implements the `DataTransform` trait of gossipsub to employ snappy compression
pub struct SnappyTransform {
    /// Sets the maximum size we allow gossipsub messages to decompress to.
    max_uncompressed_len: usize,
    /// Sets the maximum size we allow for compressed gossipsub message data.
    max_compressed_len: usize,
}

impl SnappyTransform {
    pub fn new(max_uncompressed_len: usize, max_compressed_len: usize) -> Self {
        SnappyTransform {
            max_uncompressed_len,
            max_compressed_len,
        }
    }
}

impl gossipsub::DataTransform for SnappyTransform {
    // Provides the snappy decompression from RawGossipsubMessages
    fn inbound_transform(
        &self,
        raw_message: gossipsub::RawMessage,
    ) -> Result<gossipsub::Message, std::io::Error> {
        // first check the size of the compressed payload
        if raw_message.data.len() > self.max_compressed_len {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "ssz_snappy encoded data > max_compressed_len",
            ));
        }
        // check the length of the uncompressed bytes
        let len = decompress_len(&raw_message.data)?;
        if len > self.max_uncompressed_len {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "ssz_snappy decoded data > MAX_PAYLOAD_SIZE",
            ));
        }

        let mut decoder = Decoder::new();
        let decompressed_data = decoder.decompress_vec(&raw_message.data)?;

        // Build the GossipsubMessage struct
        Ok(gossipsub::Message {
            source: raw_message.source,
            data: decompressed_data,
            sequence_number: raw_message.sequence_number,
            topic: raw_message.topic,
        })
    }

    /// Provides the snappy compression logic to gossipsub.
    fn outbound_transform(
        &self,
        _topic: &TopicHash,
        data: Vec<u8>,
    ) -> Result<Vec<u8>, std::io::Error> {
        // Currently we are not employing topic-based compression. Everything is expected to be
        // snappy compressed.
        if data.len() > self.max_uncompressed_len {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "ssz_snappy Encoded data > MAX_PAYLOAD_SIZE",
            ));
        }
        let mut encoder = Encoder::new();
        encoder.compress_vec(&data).map_err(Into::into)
    }
}

impl PubsubMessage {
    /// Returns the topics that each pubsub message will be sent across, given a supported
    /// gossipsub encoding and fork version.
    pub fn topics(&self, encoding: GossipEncoding, fork_digest: ForkDigest) -> Vec<GossipTopic> {
        vec![GossipTopic::new(self.kind(), encoding, fork_digest)]
    }

    /// Returns the kind of gossipsub topic associated with the message.
    pub fn kind(&self) -> GossipKind {
        match self {
            PubsubMessage::Block(_) => GossipKind::Block,
            PubsubMessage::Attestation(subnet, _) => GossipKind::Attestation(*subnet),
            PubsubMessage::AggregatedAttestation(_) => GossipKind::AggregatedAttestation,
        }
    }

    /// This decodes `data` into a `PubsubMessage` given a topic.
    /* Note: This is assuming we are not hashing topics. If we choose to hash topics, these will
     * need to be modified.
     */
    pub fn decode(topic: &TopicHash, data: &[u8]) -> Result<Self, String> {
        match GossipTopic::decode(topic.as_str()) {
            Err(_) => Err(format!("Unknown gossipsub topic: {:?}", topic)),
            Ok(gossip_topic) => {
                // All topics are currently expected to be compressed and decompressed with snappy.
                // This is done in the `SnappyTransform` struct.
                // Therefore compression has already been handled for us by the time we are
                // decoding the objects here.

                // the ssz decoders
                match gossip_topic.kind() {
                    GossipKind::Block => {
                        let block = SignedBlockWithAttestation::from_ssz_default(data)
                            .map_err(|e| format!("{e:?}"))?;

                        Ok(PubsubMessage::Block(Arc::new(block)))
                    }

                    GossipKind::Attestation(subnet) => {
                        let attestation = SignedAttestation::from_ssz_default(data)
                            .map_err(|e| format!("{e:?}"))?;

                        Ok(PubsubMessage::Attestation(*subnet, Arc::new(attestation)))
                    }

                    GossipKind::AggregatedAttestation => {
                        let aggregated_attestation =
                            SignedAggregatedAttestation::from_ssz_default(data)
                                .map_err(|e| format!("{e:?}"))?;

                        Ok(PubsubMessage::AggregatedAttestation(Arc::new(
                            aggregated_attestation,
                        )))
                    }
                }
            }
        }
    }

    /// Encodes a `PubsubMessage` based on the topic encodings. The first known encoding is used. If
    /// no encoding is known, and error is returned.
    pub fn encode(&self, _encoding: GossipEncoding) -> Result<Vec<u8>, WriteError> {
        // Currently do not employ encoding strategies based on the topic. All messages are ssz
        // encoded.
        // Also note, that the compression is handled by the `SnappyTransform` struct. Gossipsub will compress the
        // messages for us.
        match &self {
            PubsubMessage::Block(data) => data.to_ssz(),
            PubsubMessage::Attestation(_, data) => data.to_ssz(),
            PubsubMessage::AggregatedAttestation(data) => data.to_ssz(),
        }
    }
}

impl fmt::Display for PubsubMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PubsubMessage::Block(block) => write!(
                f,
                "Block: slot: {}, proposer_index: {}",
                block.message.block.slot.0, block.message.block.proposer_index,
            ),
            PubsubMessage::Attestation(subnet_id, attestation) => write!(
                f,
                "Attestation: subnet_id: {}, attestation_slot: {}, validator_index: {}",
                subnet_id, attestation.message.slot.0, attestation.validator_id,
            ),
            PubsubMessage::AggregatedAttestation(att) => {
                write!(f, "Aggregated Attestation: slot: {}", att.data.slot.0)
            }
        }
    }
}
