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

use crate::{
    chain::ChainMessage,
    clock::{Interval, Tick},
    simulation::{Effect, Event, Service, ServiceInput, ServiceOutput},
};

/// Messages that ValidatorService receives (input to the state machine).
#[derive(Debug, Clone)]
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

/// XMSS key manager for validator signing operations.
///
/// Loads and caches secret keys from disk, providing signing operations
/// for block proposals and attestations.
#[derive(Clone)]
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
/// the Store directly — all store data arrives via `ValidatorMessage::SlotData`
/// and `ValidatorMessage::BlockProduced`, and all store mutations are requested
/// via `ChainMessage::ProduceBlock` and `ChainMessage::ProcessAttestation`.
#[derive(Clone)]
pub struct ValidatorService {
    config: ValidatorConfig,
    key_manager: KeyManager,
}

impl ValidatorService {
    #[must_use]
    pub fn new(config: ValidatorConfig, key_manager: KeyManager) -> Self {
        info!(
            indices = ?config.validator_indices,
            "ValidatorService initialized",
        );

        Self {
            config,
            key_manager,
        }
    }

    /// Sign attestation data with the validator's XMSS key, falling back to a
    /// zero signature if signing fails.
    fn sign_attestation(&self, data: &AttestationData, validator_index: u64) -> Signature {
        let message = data.hash_tree_root();
        let epoch = data.slot.0 as u32;

        match self.key_manager.sign(validator_index, epoch, message) {
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
    }
}

impl Service for ValidatorService {
    type Message = ValidatorMessage;

    fn handle_input(&mut self, input: ServiceInput<Self::Message>) -> ServiceOutput {
        match input {
            // ── Block proposal flow ──────────────────────────────────────────
            //
            // Tick fires at interval 0: ask chain for current slot state.
            // The tick is sent to chain first (FIFO), so on_tick runs before
            // our GetSlotData query is processed — no explicit synchronization needed.
            ServiceInput::Event(Event::Tick(Tick {
                slot,
                interval: Interval::BlockProposal,
            })) => ServiceOutput::chain_message(ChainMessage::GetSlotData {
                slot: Slot(slot),
                interval: Interval::BlockProposal,
            }),

            // Chain responds with slot state: decide if we're the proposer.
            // Proposer is selected round-robin: slot % num_validators.
            ServiceInput::Message(ValidatorMessage::SlotData {
                slot,
                interval: Interval::BlockProposal,
                num_validators,
                ..
            }) => {
                assert!(num_validators > 0, "Validators cannot be empty");

                let proposer_idx = slot.0 % num_validators;

                if !self.config.validator_indices.contains(&proposer_idx) {
                    // we don't manage this proposal
                    return ServiceOutput::none();
                }

                info!(
                    slot = slot.0,
                    proposer = proposer_idx,
                    "Block proposal duty: requesting block production"
                );

                ServiceOutput::chain_message(ChainMessage::ProduceBlock { slot, proposer_idx })
            }

            // ChainService built the block; wrap it with proposer attestation
            // and broadcast to the network.
            ServiceInput::Message(ValidatorMessage::BlockProduced {
                block,
                block_root,
                signatures,
                attestation_data,
            }) => {
                let proposer_idx = block.proposer_index;
                let slot = block.slot;

                let proposer_attestation = Attestation {
                    validator_id: proposer_idx,
                    data: attestation_data,
                };
                let proposer_signature =
                    self.sign_attestation(&proposer_attestation.data, proposer_idx);

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

                // Feed the proposer's own attestation into fork choice, then
                // broadcast the signed block to the network.
                let proposer_signed_att = SignedAttestation {
                    validator_id: proposer_idx,
                    message: proposer_attestation.data,
                    signature: proposer_signature,
                };

                ServiceOutput::chain_message(ChainMessage::ProcessAttestation(proposer_signed_att))
                    .with_effect(Effect::GossipBlock(signed_block))
            }

            // ── Attestation flow ─────────────────────────────────────────────
            //
            // Tick fires at interval 1: ask chain for current slot state.
            ServiceInput::Event(Event::Tick(Tick {
                slot,
                interval: Interval::AttestationBroadcast,
            })) => ServiceOutput::chain_message(ChainMessage::GetSlotData {
                slot: Slot(slot),
                interval: Interval::AttestationBroadcast,
            }),

            // Chain responds: every validator attests exactly once per slot.
            // Proposers already bundled their attestation inside the block at
            // interval 0, so they are skipped here to avoid double-attestation.
            ServiceInput::Message(ValidatorMessage::SlotData {
                slot,
                interval: Interval::AttestationBroadcast,
                num_validators,
                attestation_data,
            }) if num_validators > 0 => {
                let proposer_idx = slot.0 % num_validators;
                let mut output = ServiceOutput::none();

                for &validator_idx in &self.config.validator_indices {
                    if validator_idx == proposer_idx {
                        // Proposer already attested within their block.
                        debug!(
                            slot = slot.0,
                            validator = validator_idx,
                            "Skipping proposer attestation (bundled in block)"
                        );
                        continue;
                    }

                    let signature = self.sign_attestation(&attestation_data, validator_idx);
                    let signed_att = SignedAttestation {
                        validator_id: validator_idx,
                        message: attestation_data.clone(),
                        signature,
                    };

                    info!(
                        slot = slot.0,
                        validator = validator_idx,
                        "Attestation produced"
                    );

                    // Feed into local fork choice and broadcast to the network.
                    output = output
                        .with_chain_message(ChainMessage::ProcessAttestation(signed_att.clone()))
                        .with_effect(Effect::GossipAttestation(signed_att));
                }

                output
            }

            // All other inputs: nothing to do.
            //
            // This covers:
            // - `Event::Tick` at intervals other than BlockProposal and AttestationBroadcast.
            // - `Event::Network` — validators do not act on raw network events;
            //   the ChainService integrates them into the store first.
            // - `ValidatorMessage::SlotData` at intervals we don't act on.
            ServiceInput::Event(_) | ServiceInput::Message(ValidatorMessage::SlotData { .. }) => {
                ServiceOutput::none()
            }
        }
    }
}
