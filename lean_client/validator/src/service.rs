use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use chain::SlotClock;
use containers::{
    Attestation, AttestationData, Block, BlockSignatures, BlockWithAttestation, Checkpoint,
    SignedAttestation, SignedBlockWithAttestation, Slot,
};
use fork_choice::{
    handlers::{on_attestation, on_block},
    store::{Store, get_vote_target, produce_block_with_signatures},
};
use metrics::METRICS;
use networking::types::OutboundP2pRequest;
use ssz::SszHash;
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep};
use tracing::{debug, info, warn};
use xmss::Signature;

use crate::{ValidatorConfig, keys::KeyManager};

/// Drives validator duties based on the slot clock.
///
/// Monitors interval boundaries and triggers block production or attestation
/// creation when scheduled.
pub struct ValidatorService {
    /// Configuration specifying which validators we control
    config: ValidatorConfig,

    /// Total number of validators in the network
    num_validators: u64,

    /// Shared reference to the forkchoice store
    store: Arc<RwLock<Store>>,

    /// Slot clock for time calculation
    clock: SlotClock,

    /// Channel for sending outbound P2P requests (blocks and attestations)
    outbound_sender: mpsc::UnboundedSender<OutboundP2pRequest>,

    /// Key manager for signing (optional - if None, uses zero signatures)
    key_manager: Option<KeyManager>,

    /// Whether the service is running
    running: Arc<AtomicBool>,

    /// Counter for produced blocks
    blocks_produced: Arc<AtomicU64>,

    /// Counter for produced attestations
    attestations_produced: Arc<AtomicU64>,
}

impl ValidatorService {
    /// Create a new ValidatorService.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: ValidatorConfig,
        num_validators: u64,
        store: Arc<RwLock<Store>>,
        clock: SlotClock,
        outbound_sender: mpsc::UnboundedSender<OutboundP2pRequest>,
        key_manager: Option<KeyManager>,
    ) -> Self {
        info!(
            node_id = %config.node_id,
            indices = ?config.validator_indices,
            total_validators = num_validators,
            has_keys = key_manager.is_some(),
            "ValidatorService initialized"
        );

        METRICS.get().map(|metrics| {
            metrics
                .lean_validators_count
                .set(config.validator_indices.len() as i64)
        });

        Self {
            config,
            num_validators,
            store,
            clock,
            outbound_sender,
            key_manager,
            running: Arc::new(AtomicBool::new(false)),
            blocks_produced: Arc::new(AtomicU64::new(0)),
            attestations_produced: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Main loop - check duties every interval.
    ///
    /// The loop:
    /// 1. Sleeps until the next interval boundary
    /// 2. Checks current interval within the slot
    /// 3. Triggers appropriate duties
    /// 4. Repeats until stopped
    ///
    /// NOTE: We track the last handled interval to avoid skipping intervals.
    /// If duty processing takes time and we end up in a new interval, we
    /// handle that interval immediately instead of sleeping past it.
    pub async fn run(&self) -> Result<()> {
        self.running.store(true, Ordering::SeqCst);
        let mut last_handled_total_interval: Option<u64> = None;

        info!("ValidatorService started");

        while self.running.load(Ordering::SeqCst) {
            // Get current total interval count (not just within-slot)
            let total_interval = self.clock.total_intervals();

            // If we've already handled this interval, sleep until the next boundary
            let already_handled = last_handled_total_interval
                .map(|last| total_interval <= last)
                .unwrap_or(false);

            if already_handled {
                self.sleep_until_next_interval().await;
                let new_total_interval = self.clock.total_intervals();

                // Check if stopped during sleep
                if !self.running.load(Ordering::SeqCst) {
                    break;
                }

                // Skip if still same interval
                if let Some(last) = last_handled_total_interval {
                    if new_total_interval <= last {
                        continue;
                    }
                }
            }

            // Skip if we have no validators to manage
            if self.config.validator_indices.is_empty() {
                last_handled_total_interval = Some(total_interval);
                continue;
            }

            // Get current slot and interval
            //
            // Interval determines which duty type to check:
            // - Interval 0: Block production
            // - Interval 1: Attestation production
            let slot = self.clock.current_slot();
            let interval = self.clock.current_interval();

            match interval {
                0 => {
                    // Block production interval
                    //
                    // Check if any of our validators is the proposer
                    self.maybe_produce_block(slot).await;
                }
                1 => {
                    // Attestation interval
                    //
                    // All validators should attest to current head
                    self.produce_attestations(slot).await;
                }
                _ => {
                    // Intervals 2-3 have no validator duties
                }
            }

            // Mark this interval as handled
            last_handled_total_interval = Some(total_interval);
        }

        info!("ValidatorService stopped");
        Ok(())
    }

    /// Produce a block if we are the proposer for this slot.
    ///
    /// Checks the proposer schedule against our validator registry.
    /// If one of our validators should propose, produces and emits the block.
    ///
    /// The proposer's attestation is bundled into the block rather than
    /// broadcast separately at interval 1. This ensures the proposer's vote
    /// is included without network round-trip delays.
    async fn maybe_produce_block(&self, slot: u64) {
        if self.num_validators == 0 {
            return;
        }

        let proposer_index = slot % self.num_validators;

        // Check if this proposer is one of ours
        if !self.config.is_assigned(proposer_index) {
            return;
        }

        info!(
            slot = slot,
            proposer = proposer_index,
            "Our turn to propose"
        );

        // Produce the block
        let signed_block = {
            let mut store = self.store.write().expect("Store lock poisoned");

            match produce_block_with_signatures(&mut store, Slot(slot), proposer_index) {
                Ok((block_root, block, signatures)) => {
                    // Create proposer attestation
                    let proposer_attestation_data = get_vote_target(&store);
                    let head_block = store
                        .blocks
                        .get(&store.head)
                        .expect("Head block must exist");
                    let head_checkpoint = Checkpoint {
                        root: store.head,
                        slot: head_block.slot,
                    };

                    let proposer_attestation = Attestation {
                        validator_id: proposer_index,
                        data: AttestationData {
                            slot: Slot(slot),
                            head: head_checkpoint,
                            target: proposer_attestation_data,
                            source: store.latest_justified.clone(),
                        },
                    };

                    // Sign the proposer attestation
                    let proposer_signature = self.sign_attestation_data(
                        &proposer_attestation.data,
                        proposer_index,
                        slot,
                    );

                    // Convert signatures to PersistentList
                    let attestation_signatures = {
                        let mut list = ssz::PersistentList::default();
                        for proof in signatures {
                            list.push(proof).expect("Failed to add signature");
                        }
                        list
                    };

                    let signed = SignedBlockWithAttestation {
                        message: BlockWithAttestation {
                            block,
                            proposer_attestation,
                        },
                        signature: BlockSignatures {
                            attestation_signatures,
                            proposer_signature,
                        },
                    };

                    info!(
                        slot = slot,
                        proposer = proposer_index,
                        block_root = %format!("{:x}", block_root),
                        "Block built successfully"
                    );

                    Some(signed)
                }
                Err(e) => {
                    warn!(
                        slot = slot,
                        proposer = proposer_index,
                        error = %e,
                        "Failed to produce block"
                    );
                    None
                }
            }
        };

        if let Some(signed_block) = signed_block {
            // Process our own block
            {
                let mut store = self.store.write().expect("Store lock poisoned");
                match on_block(&mut store, signed_block.clone()) {
                    Ok(()) => {
                        info!("Own block processed successfully");
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to process own block");
                        return;
                    }
                }
            }

            // Process proposer attestation as if from gossip
            {
                let mut store = self.store.write().expect("Store lock poisoned");
                let proposer_attestation = SignedAttestation {
                    validator_id: signed_block.message.proposer_attestation.validator_id,
                    message: signed_block.message.proposer_attestation.data.clone(),
                    signature: signed_block.signature.proposer_signature.clone(),
                };

                if let Err(e) = on_attestation(&mut store, proposer_attestation, false) {
                    warn!(error = %e, "Failed to process proposer attestation");
                }
            }

            // Increment counter and metrics
            self.blocks_produced.fetch_add(1, Ordering::Relaxed);
            // TODO: Add blocks_proposed metric
            // METRICS.get().map(|m| m.blocks_proposed.inc());

            // Gossip the block
            if let Err(e) = self
                .outbound_sender
                .send(OutboundP2pRequest::GossipBlockWithAttestation(signed_block))
            {
                warn!(error = %e, "Failed to gossip block");
            }
        }
    }

    /// Produce attestations for all non-proposer validators we control.
    ///
    /// Every validator attests exactly once per slot. Since proposers already
    /// bundled their attestation inside the block at interval 0, they are
    /// skipped here to prevent double-attestation.
    async fn produce_attestations(&self, slot: u64) {
        if self.num_validators == 0 {
            return;
        }

        let proposer_index = slot % self.num_validators;

        // Get attestation data from store
        let (attestation_data, validator_indices) = {
            let store = self.store.read().expect("Store lock poisoned");

            let vote_target = get_vote_target(&store);

            let head_block = match store.blocks.get(&store.head) {
                Some(b) => b,
                None => {
                    warn!("Head block not found, skipping attestations");
                    return;
                }
            };

            let head_checkpoint = Checkpoint {
                root: store.head,
                slot: head_block.slot,
            };

            let data = AttestationData {
                slot: Slot(slot),
                head: head_checkpoint,
                target: vote_target,
                source: store.latest_justified.clone(),
            };

            // Collect our validator indices, skipping the proposer
            let indices: Vec<u64> = self
                .config
                .validator_indices
                .iter()
                .copied()
                .filter(|&idx| idx != proposer_index)
                .collect();

            (data, indices)
        };

        // Produce and gossip attestations
        for validator_index in validator_indices {
            let signature = self.sign_attestation_data(&attestation_data, validator_index, slot);

            let signed_attestation = SignedAttestation {
                validator_id: validator_index,
                message: attestation_data.clone(),
                signature,
            };

            // Process our own attestation
            {
                let mut store = self.store.write().expect("Store lock poisoned");
                if let Err(e) = on_attestation(&mut store, signed_attestation.clone(), false) {
                    warn!(
                        validator = validator_index,
                        error = %e,
                        "Failed to process own attestation"
                    );
                    continue;
                }
            }

            // Increment counter and metrics
            self.attestations_produced.fetch_add(1, Ordering::Relaxed);
            // TODO: Add attestations_produced metric
            // METRICS.get().map(|m| m.attestations_produced.inc());

            // Gossip the attestation
            if let Err(e) = self
                .outbound_sender
                .send(OutboundP2pRequest::GossipAttestation(signed_attestation))
            {
                warn!(
                    validator = validator_index,
                    error = %e,
                    "Failed to gossip attestation"
                );
            } else {
                info!(
                    slot = slot,
                    validator = validator_index,
                    "Attestation produced and gossiped"
                );
            }
        }
    }

    /// Sign attestation data using XMSS or zero signature.
    fn sign_attestation_data(
        &self,
        attestation_data: &AttestationData,
        validator_index: u64,
        slot: u64,
    ) -> Signature {
        if let Some(ref key_manager) = self.key_manager {
            let message = attestation_data.hash_tree_root();
            let epoch = slot as u32;

            match key_manager.sign(validator_index, epoch, message) {
                Ok(sig) => {
                    debug!(
                        validator = validator_index,
                        slot = slot,
                        "Signed attestation"
                    );
                    sig
                }
                Err(e) => {
                    warn!(
                        validator = validator_index,
                        error = %e,
                        "Failed to sign attestation, using zero signature"
                    );
                    Signature::default()
                }
            }
        } else {
            Signature::default()
        }
    }

    /// Sleep until the next interval boundary.
    ///
    /// Uses the clock to calculate precise sleep duration.
    async fn sleep_until_next_interval(&self) {
        let sleep_time = self.clock.seconds_until_next_interval();
        if sleep_time > 0.0 {
            sleep(Duration::from_secs_f64(sleep_time)).await;
        }
    }

    /// Stop the service.
    ///
    /// Sets the running flag to false, causing the main loop to exit
    /// after completing its current sleep cycle.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        info!("ValidatorService stop requested");
    }

    /// Check if the service is currently running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Get total blocks produced since creation.
    pub fn blocks_produced(&self) -> u64 {
        self.blocks_produced.load(Ordering::Relaxed)
    }

    /// Get total attestations produced since creation.
    pub fn attestations_produced(&self) -> u64 {
        self.attestations_produced.load(Ordering::Relaxed)
    }
}
