//! Chain service that drives the consensus clock and owns the fork choice store.
//!
//! Every 4 seconds (1 slot), the forkchoice store processes 4 intervals:
//!
//! - Interval 0: Block proposal window
//! - Interval 1: Attestation broadcast window
//! - Interval 2: Safe target update
//! - Interval 3: Accept new attestations into fork choice
//!
//! The `ChainService` is the heartbeat. It receives tick events, advances the
//! store, and notifies other services of the current state via Messages.
//!
//! It also handles requests from other services (block production, attestation
//! processing) since it is the sole owner of the Store.

use containers::{AttestationData, Checkpoint, SignedAttestation, Slot};
use fork_choice::{
    handlers::{on_attestation, on_tick},
    store::{
        SECONDS_PER_INTERVAL, SECONDS_PER_SLOT, Store, get_vote_target,
        produce_block_with_signatures,
    },
};
use tracing::{debug, info, warn};

use crate::{
    clock::{Interval, Tick},
    simulation::{Event, Service, ServiceInput, ServiceOutput},
    validator::ValidatorMessage,
};

/// Messages that ChainService receives (input to the state machine).
#[derive(Debug, Clone)]
pub enum ChainMessage {
    /// ValidatorService → ChainService: pull slot state to decide duties.
    GetSlotData { slot: Slot, interval: Interval },

    /// ValidatorService → ChainService: request block production.
    ProduceBlock { slot: Slot, proposer_idx: u64 },

    /// ValidatorService → ChainService: process attestation in the store.
    ProcessAttestation(SignedAttestation),
}

/// Drives the consensus clock and owns the forkchoice store.
///
/// All store mutations happen here. Other services request state changes
/// through Messages.
pub struct ChainService {
    store: Store,
}

impl ChainService {
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Construct `AttestationData` from the current store state.
    ///
    /// Per spec: head checkpoint from current head, target from
    /// `get_vote_target`, source from `latest_justified`.
    fn attestation_data(&self, slot: Slot) -> AttestationData {
        let head_block = &self.store.blocks[&self.store.head];
        let head_checkpoint = Checkpoint {
            root: self.store.head,
            slot: head_block.slot,
        };

        AttestationData {
            slot,
            head: head_checkpoint,
            target: get_vote_target(&self.store),
            source: self.store.latest_justified.clone(),
        }
    }
}

impl Service for ChainService {
    type Message = ChainMessage;

    fn handle_input(&mut self, input: ServiceInput<Self::Message>) -> ServiceOutput {
        match input {
            // ── Tick flow ────────────────────────────────────────────────────
            //
            // Every interval: advance the store clock only.
            // ValidatorService drives its own duties by sending GetSlotData.
            ServiceInput::Event(Event::Tick(Tick { slot, interval })) => {
                let genesis_time = self.store.config.genesis_time;
                let interval_index = u64::from(interval as u8);
                let current_time =
                    genesis_time + slot * SECONDS_PER_SLOT + interval_index * SECONDS_PER_INTERVAL;

                on_tick(&mut self.store, current_time, false);

                debug!(slot, interval = ?interval, store_time = self.store.time, "Chain tick processed");

                ServiceOutput::none()
            }

            // ── GetSlotData flow ─────────────────────────────────────────────
            //
            // ValidatorService pulls slot state to decide its duties.
            // Called after the tick for the same slot/interval has been processed,
            // so on_tick has already run and the store is up to date.
            ServiceInput::Message(ChainMessage::GetSlotData { slot, interval }) => {
                let num_validators = self
                    .store
                    .states
                    .get(&self.store.head)
                    .map(|s| s.validators.len_u64())
                    .unwrap_or(0);

                ServiceOutput::validator_message(ValidatorMessage::SlotData {
                    slot,
                    interval,
                    num_validators,
                    attestation_data: self.attestation_data(slot),
                })
            }

            // ── Block production flow ────────────────────────────────────────
            //
            // ValidatorService determined it's the proposer and asks us to
            // build a block. We insert it into the store, then hand it back
            // with fresh attestation data so the proposer can sign and gossip.
            ServiceInput::Message(ChainMessage::ProduceBlock { slot, proposer_idx }) => {
                match produce_block_with_signatures(&mut self.store, slot, proposer_idx) {
                    Ok((block_root, block, signatures)) => {
                        info!(
                            slot = slot.0,
                            block_root = %format_args!("0x{block_root:x}"),
                            proposer = proposer_idx,
                            "Block produced and stored",
                        );

                        // Recompute attestation data after block insertion so the
                        // proposer's attestation reflects the updated chain head.
                        ServiceOutput::validator_message(ValidatorMessage::BlockProduced {
                            block,
                            block_root,
                            signatures,
                            attestation_data: self.attestation_data(slot),
                        })
                    }
                    Err(err) => {
                        warn!(%err, slot = slot.0, proposer = proposer_idx, "Failed to produce block");
                        ServiceOutput::none()
                    }
                }
            }

            // ── Attestation flow ─────────────────────────────────────────────
            //
            // ValidatorService produced an attestation; feed it into fork
            // choice. No response needed — fork choice updates silently.
            ServiceInput::Message(ChainMessage::ProcessAttestation(att)) => {
                let validator_id = att.validator_id;
                if let Err(err) = on_attestation(&mut self.store, att, false) {
                    warn!(%err, validator = validator_id, "Failed to process attestation in store");
                }
                ServiceOutput::none()
            }
        }
    }
}
