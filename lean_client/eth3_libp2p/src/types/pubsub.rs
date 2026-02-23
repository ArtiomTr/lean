//! Handles the encoding and decoding of pubsub messages.

use crate::TopicHash;
use crate::types::{GossipEncoding, GossipKind, GossipTopic};
use containers::{SignedAttestation, SignedAggregatedAttestation, SignedBlockWithAttestation, Slot};
use snap::raw::{Decoder, Encoder, decompress_len};
use ssz::{SszReadDefault, SszWrite as _, WriteError};
use std::io::{Error, ErrorKind};
use std::sync::Arc;
use types::phase0::primitives::ForkDigest;

#[derive(Debug, Clone, PartialEq)]
pub enum PubsubMessage {
    /// Gossipsub message providing notification of a new block.
    Block(Arc<SignedBlockWithAttestation>),
    /// Gossipsub message providing notification of an aggregate attestation.
    AggregateAndProofAttestation(Arc<SignedAggregatedAttestation>),
    /// Gossipsub message providing notification of a raw un-aggregated attestation with its subnet id.
    Attestation(u64, Arc<SignedAttestation>),
}

// Implements the `DataTransform` trait of gossipsub to employ snappy compression
pub struct SnappyTransform {
    max_uncompressed_len: usize,
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
    fn inbound_transform(
        &self,
        raw_message: gossipsub::RawMessage,
    ) -> Result<gossipsub::Message, std::io::Error> {
        if raw_message.data.len() > self.max_compressed_len {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "ssz_snappy encoded data > max_compressed_len",
            ));
        }
        let len = decompress_len(&raw_message.data)?;
        if len > self.max_uncompressed_len {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "ssz_snappy decoded data > MAX_PAYLOAD_SIZE",
            ));
        }

        let mut decoder = Decoder::new();
        let decompressed_data = decoder.decompress_vec(&raw_message.data)?;

        Ok(gossipsub::Message {
            source: raw_message.source,
            data: decompressed_data,
            sequence_number: raw_message.sequence_number,
            topic: raw_message.topic,
        })
    }

    fn outbound_transform(
        &self,
        _topic: &TopicHash,
        data: Vec<u8>,
    ) -> Result<Vec<u8>, std::io::Error> {
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
    /// gossipsub encoding and fork digest.
    pub fn topics(&self, encoding: GossipEncoding, fork_digest: ForkDigest) -> Vec<GossipTopic> {
        vec![GossipTopic::new(self.kind(), encoding, fork_digest)]
    }

    pub fn kind(&self) -> GossipKind {
        match self {
            PubsubMessage::Block(_) => GossipKind::BeaconBlock,
            PubsubMessage::AggregateAndProofAttestation(_) => GossipKind::BeaconAggregateAndProof,
            PubsubMessage::Attestation(subnet_id, _) => GossipKind::Attestation(*subnet_id),
        }
    }

    pub fn decode(
        topic: &TopicHash,
        data: &[u8],
    ) -> Result<Self, String> {
        match GossipTopic::decode(topic.as_str()) {
            Err(_) => Err(format!("Unknown gossipsub topic: {:?}", topic)),
            Ok(gossip_topic) => {
                match gossip_topic.kind() {
                    GossipKind::BeaconAggregateAndProof => {
                        let agg_and_proof = SignedAggregatedAttestation::from_ssz_default(data)
                            .map_err(|e| format!("{:?}", e))?;
                        Ok(PubsubMessage::AggregateAndProofAttestation(Arc::new(agg_and_proof)))
                    }
                    GossipKind::Attestation(subnet_id) => {
                        let attestation = SignedAttestation::from_ssz_default(data)
                            .map_err(|e| format!("{:?}", e))?;
                        Ok(PubsubMessage::Attestation(*subnet_id, Arc::new(attestation)))
                    }
                    GossipKind::BeaconBlock => {
                        let block = SignedBlockWithAttestation::from_ssz_default(data)
                            .map_err(|e| format!("{:?}", e))?;
                        Ok(PubsubMessage::Block(Arc::new(block)))
                    }
                }
            }
        }
    }

    pub fn encode(&self, _encoding: GossipEncoding) -> Result<Vec<u8>, WriteError> {
        match &self {
            PubsubMessage::Block(data) => data.to_ssz(),
            PubsubMessage::AggregateAndProofAttestation(data) => data.to_ssz(),
            PubsubMessage::Attestation(_, attestation) => attestation.to_ssz(),
        }
    }
}

impl std::fmt::Display for PubsubMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PubsubMessage::Block(block) => write!(
                f,
                "Block: slot: {}",
                block.message.block.slot
            ),
            PubsubMessage::AggregateAndProofAttestation(att) => write!(
                f,
                "Aggregate and Proof: slot: {}",
                att.attestation.data.slot
            ),
            PubsubMessage::Attestation(subnet_id, attestation) => write!(
                f,
                "Attestation: subnet_id: {}, attestation_slot: {}",
                subnet_id,
                attestation.attestation.data.slot
            ),
        }
    }
}
