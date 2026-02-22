//! Handles the encoding and decoding of pubsub messages.

use crate::TopicHash;
use crate::types::{GossipEncoding, GossipKind, GossipTopic};
use containers::{SignedAttestation, SignedAggregatedAttestation, SignedBlock, Slot};
use snap::raw::{Decoder, Encoder, decompress_len};
use ssz::{SszReadDefault, SszWrite as _, WriteError};
use std::io::{Error, ErrorKind};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub enum PubsubMessage {
    /// Gossipsub message providing notification of a new block.
    BeaconBlock(Arc<SignedBlock>),
    /// Gossipsub message providing notification of an aggregate attestation.
    AggregateAndProofAttestation(Arc<SignedAggregatedAttestation>),
    /// Gossipsub message providing notification of a raw un-aggregated attestation with its subnet id.
    Attestation(u64, Arc<SignedAttestation>),
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
    /// gossipsub encoding.
    pub fn topics(&self, encoding: GossipEncoding) -> Vec<GossipTopic> {
        vec![GossipTopic::new(self.kind(), encoding)]
    }

    /// Returns the kind of gossipsub topic associated with the message.
    pub fn kind(&self) -> GossipKind {
        match self {
            PubsubMessage::BeaconBlock(_) => GossipKind::BeaconBlock,
            PubsubMessage::AggregateAndProofAttestation(_) => GossipKind::BeaconAggregateAndProof,
            PubsubMessage::Attestation(subnet_id, _) => GossipKind::Attestation(*subnet_id),
        }
    }

    /// This decodes `data` into a `PubsubMessage` given a topic.
    pub fn decode(
        topic: &TopicHash,
        data: &[u8],
    ) -> Result<Self, String> {
        match GossipTopic::decode(topic.as_str()) {
            Err(_) => Err(format!("Unknown gossipsub topic: {:?}", topic)),
            Ok(gossip_topic) => {
                // All topics are currently expected to be compressed and decompressed with snappy.
                // This is done in the `SnappyTransform` struct.
                // Therefore compression has already been handled for us by the time we are
                // decoding the objects here.

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
                        let beacon_block = SignedBlock::from_ssz_default(data)
                            .map_err(|e| format!("{:?}", e))?;
                        Ok(PubsubMessage::BeaconBlock(Arc::new(beacon_block)))
                    }
                }
            }
        }
    }

    /// Encodes a `PubsubMessage` based on the topic encodings. The first known encoding is used. If
    /// no encoding is known, an error is returned.
    pub fn encode(&self, _encoding: GossipEncoding) -> Result<Vec<u8>, WriteError> {
        // Currently do not employ encoding strategies based on the topic. All messages are ssz
        // encoded.
        // Also note, that the compression is handled by the `SnappyTransform` struct. Gossipsub will compress the
        // messages for us.
        match &self {
            PubsubMessage::BeaconBlock(data) => data.to_ssz(),
            PubsubMessage::AggregateAndProofAttestation(data) => data.to_ssz(),
            PubsubMessage::Attestation(_, attestation) => attestation.to_ssz(),
        }
    }
}

impl std::fmt::Display for PubsubMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PubsubMessage::BeaconBlock(block) => write!(
                f,
                "Beacon Block: slot: {}",
                block.block.header.slot
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
