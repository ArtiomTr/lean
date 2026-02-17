//! Validator service for producing blocks and attestations.
//!
//! Validators are the active participants in Ethereum consensus. This service
//! drives validator duties based on Messages from ChainService:
//!
//! - **Interval 0 (Block Proposal)**: If one of our validators is the
//!   scheduled proposer, request block production from ChainService, then
//!   assemble and sign the block envelope with proposer attestation.
//!
//! - **Interval 1 (Attestation Broadcast)**: Create and sign attestations
//!   for all *non-proposer* validators we control. Proposers are skipped
//!   because they already attested within their block at interval 0.
//!
//! This service never accesses the Store directly. All store interactions
//! happen through Messages routed via the Node.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, anyhow};
use containers::{
    AggregatedSignatureProof, Attestation, AttestationData, Block, BlockSignatures,
    BlockWithAttestation, SignedAttestation, SignedBlockWithAttestation, Slot,
};
use ssz::{H256, PersistentList, SszHash as _};
use tracing::{debug, info, warn};
use xmss::{SecretKey, Signature};

use crate::{chain::ChainMessage, clock::Interval};

/// Messages that ValidatorService receives (input to the state machine).
pub enum ValidatorMessage {
    /// ChainService → ValidatorService: store state after a tick.
    ///
    /// Provides all data ValidatorService needs to decide duties.
    SlotData {
        slot: Slot,
        interval: Interval,
        num_validators: u64,
        attestation_data: AttestationData,
    },

    /// ChainService → ValidatorService: block production result.
    ///
    /// Contains the raw block and signatures for the validator to assemble
    /// into a `SignedBlockWithAttestation` with its own proposer attestation.
    BlockProduced {
        block: Block,
        block_root: H256,
        signatures: Vec<AggregatedSignatureProof>,
        attestation_data: AttestationData,
    },
}

/// What ValidatorService receives as input.
pub enum ValidatorInput {
    Message(ValidatorMessage),
}

/// What ValidatorService emits.
pub enum ValidatorOutput {
    /// Effects to EventSources.
    Effect(Effect),

    /// Messages to other services.
    ///
    /// We reuse ChainMessage directly instead of duplicating.
    Message(ChainMessage),
}

/// XMSS key manager for validator signing operations.
///
/// Loads and caches secret keys from disk, providing signing operations
/// for block proposals and attestations.
pub struct KeyManager {
    keys: HashMap<u64, SecretKey>,
}

impl KeyManager {
    /// Create a key manager and load keys for the given validator indices.
    pub fn load(keys_dir: impl AsRef<Path>, validator_indices: &[u64]) -> Result<Self> {
        let keys_dir = keys_dir.as_ref();

        anyhow::ensure!(keys_dir.exists(), "Keys directory not found: {keys_dir:?}");

        let mut keys = HashMap::with_capacity(validator_indices.len());

        for &idx in validator_indices {
            let sk_path = keys_dir.join(format!("validator_{idx}_sk.ssz"));
            let key_bytes = std::fs::read(&sk_path)
                .map_err(|e| anyhow!("Failed to read secret key {sk_path:?}: {e}"))?;

            let key = SecretKey::try_from(key_bytes.as_slice())?;
            info!(validator = idx, "Loaded XMSS secret key");
            keys.insert(idx, key);
        }

        Ok(Self { keys })
    }

    /// Sign a message hash with the validator's XMSS key.
    fn sign(&self, validator_index: u64, epoch: u32, message: H256) -> Result<Signature> {
        let key = self
            .keys
            .get(&validator_index)
            .ok_or_else(|| anyhow!("No key loaded for validator {validator_index}"))?;

        key.sign(message, epoch)
    }
}

/// Configuration specifying which validators a node controls.
#[derive(Debug, Clone)]
pub struct ValidatorConfig {
    pub validator_indices: Vec<u64>,
}

/// Drives validator duties based on Messages from ChainService.
///
/// This service is a pure deterministic state machine. It never accesses
/// the Store directly — all store data arrives via `Message::SlotData`
/// and `Message::BlockProduced`, and all store mutations are requested
/// via `Message::ProduceBlock` and `Message::ProcessAttestation`.
pub struct ValidatorService {
    config: ValidatorConfig,
    key_manager: Option<KeyManager>,
    blocks_produced: u64,
    attestations_produced: u64,
}

impl ValidatorService {
    #[must_use]
    pub fn new(config: ValidatorConfig, key_manager: Option<KeyManager>) -> Self {
        info!(
            indices = ?config.validator_indices,
            has_keys = key_manager.is_some(),
            "ValidatorService initialized",
        );

        Self {
            config,
            key_manager,
            blocks_produced: 0,
            attestations_produced: 0,
        }
    }

    #[must_use]
    pub fn blocks_produced(&self) -> u64 {
        self.blocks_produced
    }

    #[must_use]
    pub fn attestations_produced(&self) -> u64 {
        self.attestations_produced
    }

    /// Process an input and return outputs.
    ///
    /// ValidatorService only acts on Messages from ChainService, not raw
    /// Events. The ChainService translates Events into Messages containing
    /// the store state this service needs.
    pub fn handle_input(&mut self, input: ValidatorInput) -> Vec<ValidatorOutput> {
        match input {
            ValidatorInput::Message(msg) => self.handle_message(msg),
        }
    }

    fn handle_message(&mut self, msg: ValidatorMessage) -> Vec<ValidatorOutput> {
        match msg {
            ValidatorMessage::SlotData {
                slot,
                interval,
                num_validators,
                attestation_data,
            } => match interval {
                Interval::BlockProposal => self.maybe_request_block(slot, num_validators),
                Interval::AttestationBroadcast => {
                    self.produce_attestations(slot, num_validators, &attestation_data)
                }
                Interval::SafeTargetUpdate | Interval::AttestationAcceptance => vec![],
            },
            ValidatorMessage::BlockProduced {
                block,
                block_root,
                signatures,
                attestation_data,
            } => self.assemble_block(block, block_root, signatures, attestation_data),
        }
    }

    /// Check if one of our validators is the proposer and request block
    /// production from ChainService.
    fn maybe_request_block(&self, slot: Slot, num_validators: u64) -> Vec<ValidatorOutput> {
        if num_validators == 0 {
            return vec![];
        }

        // Round-robin proposer selection per spec.
        let expected_proposer = slot.0 % num_validators;

        if !self.config.validator_indices.contains(&expected_proposer) {
            return vec![];
        }

        info!(
            slot = slot.0,
            proposer = expected_proposer,
            "Block proposal duty: requesting block production",
        );

        vec![ValidatorOutput::Message(ChainMessage::ProduceBlock {
            slot,
            proposer_idx: expected_proposer,
        })]
    }

    /// Assemble the block envelope with proposer attestation and signatures.
    ///
    /// Called when ChainService responds with `BlockProduced` containing the
    /// raw block and aggregated signature proofs.
    fn assemble_block(
        &mut self,
        block: Block,
        block_root: H256,
        signatures: Vec<AggregatedSignatureProof>,
        attestation_data: AttestationData,
    ) -> Vec<ValidatorOutput> {
        let proposer_idx = block.proposer_index;
        let slot = block.slot;

        // Create proposer attestation.
        let proposer_attestation = Attestation {
            validator_id: proposer_idx,
            data: attestation_data,
        };

        // Sign the proposer attestation.
        let proposer_signature =
            self.sign_attestation_data(&proposer_attestation.data, proposer_idx);

        // Convert signature proofs to PersistentList.
        let mut attestation_signatures = PersistentList::default();
        for proof in signatures {
            attestation_signatures
                .push(proof)
                .expect("Failed to add signature proof");
        }

        let signed_block = SignedBlockWithAttestation {
            message: BlockWithAttestation {
                block,
                proposer_attestation: proposer_attestation.clone(),
            },
            signature: BlockSignatures {
                attestation_signatures,
                proposer_signature: proposer_signature.clone(),
            },
        };

        info!(
            slot = slot.0,
            block_root = %format_args!("0x{block_root:x}"),
            proposer = proposer_idx,
            "Block assembled",
        );

        self.blocks_produced += 1;

        // Request ChainService to process the proposer attestation, and
        // gossip the signed block to the network.
        let proposer_signed_att = SignedAttestation {
            validator_id: proposer_idx,
            message: proposer_attestation.data,
            signature: proposer_signature,
        };

        vec![
            ValidatorOutput::Message(ChainMessage::ProcessAttestation(proposer_signed_att)),
            ValidatorOutput::Effect(Effect::GossipBlock(signed_block)),
        ]
    }

    /// Create attestations for all non-proposer validators we control.
    ///
    /// Every validator attests exactly once per slot. Since proposers already
    /// bundled their attestation inside the block at interval 0, they are
    /// skipped here to prevent double-attestation.
    fn produce_attestations(
        &mut self,
        slot: Slot,
        num_validators: u64,
        attestation_data: &AttestationData,
    ) -> Vec<ValidatorOutput> {
        if num_validators == 0 {
            return vec![];
        }

        let proposer_idx = slot.0 % num_validators;
        let mut outputs = Vec::with_capacity(self.config.validator_indices.len() * 2);

        for &validator_idx in &self.config.validator_indices {
            // Skip the proposer: they already attested within their block.
            if validator_idx == proposer_idx {
                debug!(
                    slot = slot.0,
                    validator = validator_idx,
                    "Skipping proposer attestation (bundled in block)",
                );
                continue;
            }

            let signature = self.sign_attestation_data(attestation_data, validator_idx);

            let signed_att = SignedAttestation {
                validator_id: validator_idx,
                message: attestation_data.clone(),
                signature,
            };

            self.attestations_produced += 1;
            info!(
                slot = slot.0,
                validator = validator_idx,
                "Attestation produced"
            );

            // Request ChainService to process locally + gossip to network.
            outputs.push(ValidatorOutput::Message(ChainMessage::ProcessAttestation(
                signed_att.clone(),
            )));
            outputs.push(ValidatorOutput::Effect(Effect::GossipAttestation(
                signed_att,
            )));
        }

        outputs
    }

    /// Sign attestation data using XMSS or a zero signature if no keys loaded.
    fn sign_attestation_data(&self, data: &AttestationData, validator_index: u64) -> Signature {
        if let Some(ref km) = self.key_manager {
            let message = data.hash_tree_root();
            let epoch = data.slot.0 as u32;

            match km.sign(validator_index, epoch, message) {
                Ok(sig) => sig,
                Err(err) => {
                    warn!(
                        %err,
                        validator = validator_index,
                        "Failed to sign attestation, using zero signature",
                    );
                    Signature::default()
                }
            }
        } else {
            Signature::default()
        }
    }
}
