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

use crate::{clock::Tick, simulation::Service, validator::ValidatorMessage};

/// Messages that ChainService receives (input to the state machine).
#[derive(Debug, Clone)]
pub enum ChainMessage {
    /// ValidatorService → ChainService: request block production.
    ProduceBlock { slot: Slot, proposer_idx: u64 },

    /// ValidatorService → ChainService: process attestation in the store.
    ProcessAttestation(SignedAttestation),
}

/// What ChainService receives: an Event or a Message from another Service.
pub enum ChainInput {
    Event(Event),
    Message(ChainMessage),
}

/// What ChainService emits: messages to ValidatorService.
pub type ChainOutput = ValidatorMessage;

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

    /// Process an input and return outputs.
    pub fn handle_input(&mut self, input: ChainInput) -> Vec<ChainOutput> {
        match input {
            ChainInput::Event(Event::Tick(tick)) => self.handle_tick(tick),
            ChainInput::Message(msg) => self.handle_message(msg),
        }
    }

    /// Advance the store clock and notify ValidatorService of current state.
    fn handle_tick(&mut self, tick: Tick) -> Vec<ChainOutput> {
        // Compute Unix timestamp from tick's logical time.
        let genesis_time = self.store.config.genesis_time;
        let interval_index = u64::from(tick.interval as u8);
        let current_time =
            genesis_time + tick.slot * SECONDS_PER_SLOT + interval_index * SECONDS_PER_INTERVAL;

        on_tick(&mut self.store, current_time, false);

        debug!(
            slot = tick.slot,
            interval = ?tick.interval,
            store_time = self.store.time,
            "Chain tick processed",
        );

        // Compute validator-relevant state from head.
        let num_validators = self
            .store
            .states
            .get(&self.store.head)
            .map(|s| s.validators.len_u64())
            .unwrap_or(0);

        let attestation_data = self.produce_attestation_data(Slot(tick.slot));

        vec![ValidatorMessage::SlotData {
            slot: Slot(tick.slot),
            interval: tick.interval,
            num_validators,
            attestation_data,
        }]
    }

    fn handle_message(&mut self, msg: ChainMessage) -> Vec<ChainOutput> {
        match msg {
            ChainMessage::ProduceBlock { slot, proposer_idx } => {
                self.handle_produce_block(slot, proposer_idx)
            }
            ChainMessage::ProcessAttestation(att) => {
                let validator_id = att.validator_id;
                if let Err(err) = on_attestation(&mut self.store, att, false) {
                    warn!(%err, validator = validator_id, "Failed to process attestation in store");
                }
                vec![]
            }
        }
    }

    /// Build a block via `produce_block_with_signatures` and return the result
    /// as a `BlockProduced` message for ValidatorService to assemble.
    fn handle_produce_block(&mut self, slot: Slot, proposer_idx: u64) -> Vec<ChainOutput> {
        match produce_block_with_signatures(&mut self.store, slot, proposer_idx) {
            Ok((block_root, block, signatures)) => {
                info!(
                    slot = slot.0,
                    block_root = %format_args!("0x{block_root:x}"),
                    proposer = proposer_idx,
                    "Block produced and stored",
                );

                // Compute fresh attestation data after block insertion, so the
                // proposer's attestation reflects the updated chain head.
                let attestation_data = self.produce_attestation_data(slot);

                vec![ValidatorMessage::BlockProduced {
                    block,
                    block_root,
                    signatures,
                    attestation_data,
                }]
            }
            Err(err) => {
                warn!(%err, slot = slot.0, proposer = proposer_idx, "Failed to produce block");
                vec![]
            }
        }
    }

    /// Construct `AttestationData` from the current store state.
    ///
    /// Per spec: head checkpoint from current head, target from
    /// `get_vote_target`, source from `latest_justified`.
    fn produce_attestation_data(&self, slot: Slot) -> AttestationData {
        let head_block = &self.store.blocks[&self.store.head];
        let head_checkpoint = Checkpoint {
            root: self.store.head,
            slot: head_block.slot,
        };

        let target = get_vote_target(&self.store);
        let source = self.store.latest_justified.clone();

        AttestationData {
            slot,
            head: head_checkpoint,
            target,
            source,
        }
    }
}

impl Service for ChainService {
    type Message = ChainMessage;

    fn handle_input(
        &mut self,
        input: crate::simulation::ServiceInput<Self::Message>,
    ) -> crate::simulation::ServiceOutput {
        todo!()
    }
}
