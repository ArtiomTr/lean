use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use clock::{Interval, SystemClock};
use containers::{
    Attestation, AttestationData, BlockSignatures, BlockWithAttestation, SignedAttestation,
    SignedBlockWithAttestation, Slot,
};
use fork_choice::Store;
use metrics::METRICS;
use ssz::SszHash;
use tokio::time::sleep;
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
    clock: SystemClock,

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
        clock: SystemClock,
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
            if self.clock.checked_current_slot().is_none() {
                self.sleep_until_next_interval().await;
                continue;
            }

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
                Interval::BlockProposal => {
                    // Block production interval
                    //
                    // Check if any of our validators is the proposer
                    self.maybe_produce_block(slot).await;
                }
                Interval::AttestationBroadcast => {
                    // Attestation interval
                    //
                    // All validators should attest to current head
                    self.produce_attestations(slot).await;
                }
                _ => {
                    // Remaining intervals have no validator duties
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

            match store.produce_block_with_signatures(Slot(slot), proposer_index) {
                Ok((block_root, block, signatures)) => {
                    // Create proposer attestation
                    let proposer_attestation_data = match store.produce_attestation_data(Slot(slot))
                    {
                        Ok(data) => data,
                        Err(e) => {
                            warn!(
                                slot = slot,
                                proposer = proposer_index,
                                error = %e,
                                "Failed to create proposer attestation data"
                            );
                            return;
                        }
                    };

                    let proposer_attestation = Attestation {
                        validator_id: proposer_index,
                        data: proposer_attestation_data,
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
                match store.on_block(signed_block.clone()) {
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

                if let Err(e) = store.on_gossip_attestation(&proposer_attestation, false) {
                    warn!(error = %e, "Failed to process proposer attestation");
                }
            }

            // Increment counter and metrics
            self.blocks_produced.fetch_add(1, Ordering::Relaxed);
            // TODO: Add blocks_proposed metric
            // METRICS.get().map(|m| m.blocks_proposed.inc());

            let _ = signed_block;
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

            let data = match store.produce_attestation_data(Slot(slot)) {
                Ok(data) => data,
                Err(e) => {
                    warn!(
                        slot = slot,
                        error = %e,
                        "Failed to produce attestation data, skipping attestations"
                    );
                    return;
                }
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
                if let Err(e) = store.on_gossip_attestation(&signed_attestation, false) {
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

            info!(
                slot = slot,
                validator = validator_index,
                "Attestation produced"
            );
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
        let sleep_time = self.clock.time_until_next_interval();
        if !sleep_time.is_zero() {
            sleep(sleep_time).await;
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
