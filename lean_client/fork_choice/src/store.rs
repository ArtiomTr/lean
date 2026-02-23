// Allow modulo_one: ATTESTATION_COMMITTEE_COUNT is 1 for devnet but will increase
#![allow(clippy::modulo_one)]

use std::collections::{HashMap, HashSet};

use anyhow::{Result, anyhow, ensure};
use containers::{
    AggregatedSignatureProof, AggregationBits, Attestation, AttestationData, Block, Checkpoint,
    Config, SignatureKey, SignedAggregatedAttestation, SignedAttestation,
    SignedBlockWithAttestation, Slot, State,
};
use metrics::set_gauge_u64;
use ssz::{H256, SszHash as _};
use xmss::Signature;

pub type Interval = u64;
pub const INTERVALS_PER_SLOT: Interval = 5;
pub const SECONDS_PER_SLOT: u64 = 4;
pub const ATTESTATION_COMMITTEE_COUNT: u64 = 1;
pub const JUSTIFICATION_LOOKBACK_SLOTS: u64 = 3;

/// Forkchoice store tracking chain state and validator attestations
///
/// This is the "local view" that a node uses to run LMD GHOST. It contains:
/// - which blocks and states are known,
/// - which checkpoints are justified and finalized,
/// - which block is currently considered the head,
/// - and aggregated signature proofs for attestations that influence fork choice.
#[derive(Debug, Clone, Default)]
pub struct Store {
    /// Current time in intervals since genesis
    time: Interval,

    /// Chain configuration parameters
    config: Config,

    /// Root of the current canonical chain head block
    head: H256,

    /// Root of the current safe target for attestation
    safe_target: H256,

    /// Highest slot justified checkpoint known to the store
    latest_justified: Checkpoint,

    /// Highest slot finalized checkpoint known to the store
    latest_finalized: Checkpoint,

    /// Mapping from block root to Block objects
    blocks: HashMap<H256, Block>,

    /// Mapping from block root to State objects
    states: HashMap<H256, State>,

    /// Index of the validator running this store instance
    validator_id: Option<u64>,

    /// Per-validator XMSS signatures learned from committee attesters
    gossip_signatures: HashMap<SignatureKey, Signature>,

    /// Mapping from attestation data root to full AttestationData
    attestation_data_by_root: HashMap<H256, AttestationData>,

    /// Aggregated signature proofs pending processing (not yet in fork choice)
    latest_new_aggregated_payloads: HashMap<SignatureKey, Vec<AggregatedSignatureProof>>,

    /// Aggregated signature proofs that have been processed (actively used in fork choice)
    latest_known_aggregated_payloads: HashMap<SignatureKey, Vec<AggregatedSignatureProof>>,
}

impl Store {
    /// Initialize forkchoice store from an anchor state and block.
    ///
    /// The anchor block and its state form the starting point for fork choice.
    /// We treat this anchor as both justified and finalized.
    ///
    /// # Arguments
    /// * `anchor_state` - The state corresponding to the anchor block
    /// * `anchor_block` - A trusted block (e.g. genesis or checkpoint)
    /// * `validator_id` - Index of the validator running this store (None for non-validators)
    ///
    /// # Panics
    /// Panics if the anchor block's state root does not match the hash of the provided state.
    pub fn new(anchor_state: State, anchor_block: Block, validator_id: Option<u64>) -> Self {
        // Compute the SSZ root of the given state
        let computed_state_root = anchor_state.hash_tree_root();

        // Check that the block actually points to this state
        assert_eq!(
            anchor_block.state_root, computed_state_root,
            "Anchor block state root must match anchor state hash"
        );

        // Compute the SSZ root of the anchor block itself
        let anchor_root = anchor_block.hash_tree_root();
        let anchor_slot = anchor_block.slot;

        // Initialize checkpoints from the anchor block.
        // Both root and slot come from the anchor block directly.
        let anchor_checkpoint = Checkpoint {
            root: anchor_root,
            slot: anchor_slot,
        };

        Store {
            time: anchor_slot.0 * INTERVALS_PER_SLOT,
            config: anchor_state.config.clone(),
            head: anchor_root,
            safe_target: anchor_root,
            latest_justified: anchor_checkpoint.clone(),
            latest_finalized: anchor_checkpoint,
            blocks: [(anchor_root, anchor_block)].into(),
            states: [(anchor_root, anchor_state)].into(),
            validator_id,
            gossip_signatures: HashMap::new(),
            attestation_data_by_root: HashMap::new(),
            latest_new_aggregated_payloads: HashMap::new(),
            latest_known_aggregated_payloads: HashMap::new(),
        }
    }

    // ========== Getters ==========

    #[inline]
    pub fn time(&self) -> Interval {
        self.time
    }

    #[inline]
    pub fn config(&self) -> &Config {
        &self.config
    }

    #[inline]
    pub fn head(&self) -> H256 {
        self.head
    }

    #[inline]
    pub fn safe_target(&self) -> H256 {
        self.safe_target
    }

    #[inline]
    pub fn latest_justified(&self) -> &Checkpoint {
        &self.latest_justified
    }

    #[inline]
    pub fn latest_finalized(&self) -> &Checkpoint {
        &self.latest_finalized
    }

    #[inline]
    pub fn blocks(&self) -> &HashMap<H256, Block> {
        &self.blocks
    }

    #[inline]
    pub fn states(&self) -> &HashMap<H256, State> {
        &self.states
    }

    #[inline]
    pub fn gossip_signatures(&self) -> &HashMap<SignatureKey, Signature> {
        &self.gossip_signatures
    }

    #[inline]
    pub fn attestation_data_by_root(&self) -> &HashMap<H256, AttestationData> {
        &self.attestation_data_by_root
    }

    #[inline]
    pub fn latest_new_aggregated_payloads(
        &self,
    ) -> &HashMap<SignatureKey, Vec<AggregatedSignatureProof>> {
        &self.latest_new_aggregated_payloads
    }

    #[inline]
    pub fn latest_known_aggregated_payloads(
        &self,
    ) -> &HashMap<SignatureKey, Vec<AggregatedSignatureProof>> {
        &self.latest_known_aggregated_payloads
    }

    #[inline]
    pub fn get_block(&self, root: &H256) -> Option<&Block> {
        self.blocks.get(root)
    }

    #[inline]
    pub fn get_state(&self, root: &H256) -> Option<&State> {
        self.states.get(root)
    }

    // ========== Core Helper Methods ==========

    /// Remove attestation data that can no longer influence fork choice.
    ///
    /// An attestation becomes stale when its target checkpoint falls at or before
    /// the finalized slot. Pruning removes data from all attestation-related maps:
    /// - attestation_data_by_root
    /// - gossip_signatures
    /// - latest_new_aggregated_payloads
    /// - latest_known_aggregated_payloads
    fn prune_stale_attestation_data(&mut self) {
        let finalized_slot = self.latest_finalized.slot;

        // Collect stale roots (attestations targeting at or before finalization)
        let stale_roots: HashSet<H256> = self
            .attestation_data_by_root
            .iter()
            .filter(|(_, data)| data.target.slot <= finalized_slot)
            .map(|(root, _)| *root)
            .collect();

        if stale_roots.is_empty() {
            return;
        }

        // Remove stale entries from all attestation-related maps
        self.attestation_data_by_root
            .retain(|root, _| !stale_roots.contains(root));

        self.gossip_signatures
            .retain(|key, _| !stale_roots.contains(&key.data_root));

        self.latest_new_aggregated_payloads
            .retain(|key, _| !stale_roots.contains(&key.data_root));

        self.latest_known_aggregated_payloads
            .retain(|key, _| !stale_roots.contains(&key.data_root));
    }

    /// Validate incoming attestation before processing.
    ///
    /// Ensures the vote respects the basic laws of time and topology:
    /// 1. The blocks voted for must exist in our store.
    /// 2. A vote cannot span backwards in time (source slot > target slot).
    /// 3. The head must be at least as recent as target (head >= target).
    /// 4. Checkpoint slots must match the actual block slots.
    /// 5. A vote cannot be too far in the future (more than 1 slot ahead).
    pub fn validate_attestation(&self, attestation: &Attestation) -> Result<()> {
        let data = &attestation.data;

        // Availability Check: we cannot count a vote if we haven't seen the blocks involved.
        ensure!(
            self.blocks.contains_key(&data.source.root),
            "Unknown source block: {}",
            data.source.root
        );
        ensure!(
            self.blocks.contains_key(&data.target.root),
            "Unknown target block: {}",
            data.target.root
        );
        ensure!(
            self.blocks.contains_key(&data.head.root),
            "Unknown head block: {}",
            data.head.root
        );

        // Topology Check: source <= target <= head.
        ensure!(
            data.source.slot <= data.target.slot,
            "Source checkpoint slot must not exceed target"
        );
        ensure!(
            data.head.slot >= data.target.slot,
            "Head checkpoint must not be older than target"
        );

        // Consistency Check: checkpoint slots must match block slots.
        let source_block = &self.blocks[&data.source.root];
        let target_block = &self.blocks[&data.target.root];
        let head_block = &self.blocks[&data.head.root];
        ensure!(
            source_block.slot == data.source.slot,
            "Source checkpoint slot mismatch: block slot {} != checkpoint slot {}",
            source_block.slot.0,
            data.source.slot.0
        );
        ensure!(
            target_block.slot == data.target.slot,
            "Target checkpoint slot mismatch: block slot {} != checkpoint slot {}",
            target_block.slot.0,
            data.target.slot.0
        );
        ensure!(
            head_block.slot == data.head.slot,
            "Head checkpoint slot mismatch: block slot {} != checkpoint slot {}",
            head_block.slot.0,
            data.head.slot.0
        );

        // Time Check: attestation must not be more than 1 slot in the future.
        let current_slot = self.time / INTERVALS_PER_SLOT;
        ensure!(
            data.slot.0 <= current_slot + 1,
            "Attestation too far in future: slot {} > current {} + 1",
            data.slot.0,
            current_slot
        );

        Ok(())
    }

    // ========== Gossip Attestation Handling ==========

    /// Process a signed attestation received via gossip network.
    ///
    /// This method:
    /// 1. Validates the attestation
    /// 2. Verifies the XMSS signature
    /// 3. If current node is aggregator, stores the signature if it belongs to
    ///    the current validator's subnet
    /// 4. Stores attestation data for later extraction
    ///
    /// # Arguments
    /// * `signed_attestation` - The signed attestation from gossip
    /// * `is_aggregator` - True if current validator holds aggregator role
    pub fn on_gossip_attestation(
        &mut self,
        signed_attestation: &SignedAttestation,
        is_aggregator: bool,
    ) -> Result<()> {
        let validator_id = signed_attestation.validator_id;
        let attestation_data = &signed_attestation.message;
        let signature = &signed_attestation.signature;

        // Validate attestation first so unknown blocks are rejected cleanly
        let attestation = Attestation {
            validator_id,
            data: attestation_data.clone(),
        };
        self.validate_attestation(&attestation)?;

        // Get state for signature verification
        let target_state = self
            .get_state(&attestation_data.target.root)
            .ok_or_else(|| {
                anyhow!(
                    "No state available to verify attestation signature for target {}",
                    attestation_data.target.root
                )
            })?;

        ensure!(
            (validator_id as usize) < target_state.validators.len_usize(),
            "Validator {} not found in state",
            validator_id
        );

        let public_key = &target_state
            .validators
            .get(validator_id)
            .expect("validator not found")
            .pubkey;

        // Verify signature
        let data_root = attestation_data.hash_tree_root();
        signature
            .verify(public_key, attestation_data.slot.0 as u32, data_root)
            .map_err(|e| anyhow!("Signature verification failed: {}", e))?;

        // Store signature if aggregator with subnet filtering
        if is_aggregator && let Some(my_id) = self.validator_id {
            let my_subnet = my_id % ATTESTATION_COMMITTEE_COUNT;
            let attester_subnet = validator_id % ATTESTATION_COMMITTEE_COUNT;

            if my_subnet == attester_subnet {
                let sig_key = SignatureKey {
                    validator_id,
                    data_root,
                };
                self.gossip_signatures.insert(sig_key, signature.clone());
            }
        }

        // Store attestation data for later extraction
        self.attestation_data_by_root
            .insert(data_root, attestation_data.clone());

        Ok(())
    }

    /// Process a signed aggregated attestation received via aggregation topic.
    ///
    /// This method:
    /// 1. Verifies the aggregated attestation
    /// 2. Stores the aggregation in latest_new_aggregated_payloads
    ///
    /// # Arguments
    /// * `signed_attestation` - The signed aggregated attestation from committee aggregation
    pub fn on_gossip_aggregated_attestation(
        &mut self,
        signed_attestation: &SignedAggregatedAttestation,
    ) -> Result<()> {
        let data = &signed_attestation.data;
        let proof = &signed_attestation.proof;

        // Get validator IDs who participated
        let validator_ids = proof.get_participant_indices();

        // Get state for verification
        let target_state = self.get_state(&data.target.root).ok_or_else(|| {
            anyhow!(
                "No state available to verify committee aggregation for target {}",
                data.target.root
            )
        })?;

        // Ensure all participants exist
        for &vid in &validator_ids {
            ensure!(
                (vid as usize) < target_state.validators.len_usize(),
                "Validator {} not found in state",
                vid
            );
        }

        // Prepare public keys for verification
        let public_keys: Vec<_> = validator_ids
            .iter()
            .map(|&vid| {
                target_state
                    .validators
                    .get(vid)
                    .expect("validator not found")
                    .pubkey
                    .clone()
            })
            .collect();

        // Verify the aggregated proof
        let data_root = data.hash_tree_root();
        proof
            .verify(public_keys, data_root, data.slot.0 as u32)
            .map_err(|e| anyhow!("Committee aggregation signature verification failed: {}", e))?;

        // Store attestation data by root
        self.attestation_data_by_root
            .insert(data_root, data.clone());

        // Store in new payloads pool
        for vid in validator_ids {
            let key = SignatureKey {
                validator_id: vid,
                data_root,
            };
            self.latest_new_aggregated_payloads
                .entry(key)
                .or_default()
                .push(proof.clone());
        }

        Ok(())
    }

    // ========== Core Fork Choice Methods ==========

    pub fn get_fork_choice_head(
        &self,
        mut root: H256,
        latest_attestations: &HashMap<u64, AttestationData>,
        min_votes: usize,
    ) -> H256 {
        if root.is_zero() {
            root = self
                .blocks
                .iter()
                .min_by_key(|(_, block)| block.slot)
                .map(|(r, _)| *r)
                .expect("Error: Empty block.");
        }

        let root_slot = self.blocks[&root].slot;
        let mut vote_weights: HashMap<H256, usize> = HashMap::new();

        // Accumulate weights by walking up from each attestation's head
        for attestation_data in latest_attestations.values() {
            let mut current = attestation_data.head.root;

            while let Some(block) = self.blocks.get(&current) {
                if block.slot <= root_slot {
                    break;
                }
                *vote_weights.entry(current).or_insert(0) += 1;
                current = block.parent_root;
                if current.is_zero() {
                    break;
                }
            }
        }

        // Build adjacency tree (parent -> children), pruning low-weight branches
        let mut child_map: HashMap<H256, Vec<H256>> = HashMap::new();
        for (block_hash, block) in &self.blocks {
            if block.parent_root.is_zero() {
                continue;
            }
            if min_votes > 0 && vote_weights.get(block_hash).copied().unwrap_or(0) < min_votes {
                continue;
            }
            child_map
                .entry(block.parent_root)
                .or_default()
                .push(*block_hash);
        }

        // Greedy walk choosing heaviest child at each fork
        let mut curr = root;
        loop {
            let children = match child_map.get(&curr) {
                Some(list) if !list.is_empty() => list,
                _ => return curr,
            };

            // Choose best child: most attestations, then lexicographically highest hash
            curr = *children
                .iter()
                .max_by(|&&a, &&b| {
                    let wa = vote_weights.get(&a).copied().unwrap_or(0);
                    let wb = vote_weights.get(&b).copied().unwrap_or(0);
                    wa.cmp(&wb).then_with(|| a.cmp(&b))
                })
                .unwrap();
        }
    }

    /// Extract attestations from aggregated payloads.
    ///
    /// Given a mapping of aggregated signature proofs, extract the attestation data
    /// for each validator that participated.
    ///
    /// Returns a mapping from ValidatorIndex to AttestationData.
    fn extract_attestations_from_aggregated_payloads(
        &self,
        aggregated_payloads: &HashMap<SignatureKey, Vec<AggregatedSignatureProof>>,
    ) -> HashMap<u64, AttestationData> {
        let mut attestations: HashMap<u64, AttestationData> = HashMap::new();

        for (sig_key, proofs) in aggregated_payloads {
            let data_root = sig_key.data_root;
            let Some(attestation_data) = self.attestation_data_by_root.get(&data_root) else {
                continue;
            };

            for proof in proofs {
                let validator_ids = proof.get_participant_indices();
                for vid in validator_ids {
                    attestations
                        .entry(vid)
                        .and_modify(|existing| {
                            if attestation_data.slot > existing.slot {
                                *existing = attestation_data.clone();
                            }
                        })
                        .or_insert_with(|| attestation_data.clone());
                }
            }
        }

        attestations
    }

    pub fn get_latest_justified(states: &HashMap<H256, State>) -> Option<&Checkpoint> {
        states
            .values()
            .map(|state| &state.latest_justified)
            .max_by_key(|checkpoint| checkpoint.slot)
    }

    pub fn update_head(&mut self) {
        // Extract attestations from known aggregated payloads
        let attestations = self.extract_attestations_from_aggregated_payloads(
            &self.latest_known_aggregated_payloads.clone(),
        );

        // Compute new head using LMD-GHOST from latest justified root
        let new_head = self.get_fork_choice_head(self.latest_justified.root, &attestations, 0);
        self.head = new_head;

        let blocks = &self.blocks;
        set_gauge_u64(
            |m| &m.lean_head_slot,
            || {
                let head = blocks
                    .get(&new_head)
                    .ok_or(anyhow!("failed to get head block"))?;

                Ok(head.slot.0)
            },
        );
    }

    fn update_safe_target(&mut self) {
        let n_validators = if let Some(state) = self.states.get(&self.head) {
            state.validators.len_usize()
        } else {
            0
        };

        // Ceiling division: ceil(n * 2 / 3)
        let min_score = (n_validators * 2).div_ceil(3);

        // Merge both known and new aggregated payloads for safe target calculation.
        // At interval 3, the interval-4 migration has not yet run, so attestations
        // in the "new" pool (from block proposers and self-attestations) must be
        // included to avoid undercounting support.
        let mut all_payloads = self.latest_known_aggregated_payloads.clone();
        for (sig_key, proofs) in &self.latest_new_aggregated_payloads {
            all_payloads
                .entry(sig_key.clone())
                .or_default()
                .extend(proofs.iter().cloned());
        }

        let attestations = self.extract_attestations_from_aggregated_payloads(&all_payloads);

        let root = self.latest_justified.root;
        let new_safe_target = self.get_fork_choice_head(root, &attestations, min_score);
        self.safe_target = new_safe_target;

        let blocks = &self.blocks;
        set_gauge_u64(
            |metrics| &metrics.lean_safe_target_slot,
            || {
                let safe_target = blocks
                    .get(&new_safe_target)
                    .ok_or(anyhow!("failed to get safe target block"))?;

                Ok(safe_target.slot.0)
            },
        );
    }

    fn accept_new_attestations(&mut self) {
        // Merge latest_new_aggregated_payloads into latest_known_aggregated_payloads.
        // For the same signature key, proof lists are concatenated.
        let new_payloads: Vec<_> = self.latest_new_aggregated_payloads.drain().collect();
        for (sig_key, proofs) in new_payloads {
            self.latest_known_aggregated_payloads
                .entry(sig_key)
                .or_default()
                .extend(proofs);
        }
        self.update_head();
    }

    /// Aggregate gossip signatures into combined proofs and update the new aggregated payloads.
    ///
    /// Groups signatures by attestation data, creates an AggregatedSignatureProof per group,
    /// updates latest_new_aggregated_payloads, and returns the resulting aggregated attestations.
    pub fn aggregate_committee_signatures(&mut self) -> Vec<SignedAggregatedAttestation> {
        let head_state = match self.states.get(&self.head).cloned() {
            Some(s) => s,
            None => return vec![],
        };

        // Group validator IDs by attestation data root
        let mut groups: HashMap<H256, (AttestationData, Vec<u64>)> = HashMap::new();
        for sig_key in self.gossip_signatures.keys() {
            let data_root = sig_key.data_root;
            let Some(attestation_data) = self.attestation_data_by_root.get(&data_root) else {
                continue;
            };
            groups
                .entry(data_root)
                .and_modify(|(_, ids)| ids.push(sig_key.validator_id))
                .or_insert_with(|| (attestation_data.clone(), vec![sig_key.validator_id]));
        }

        let mut new_aggregates = vec![];
        let mut to_remove = vec![];

        for (data_root, (attestation_data, validator_ids)) in &groups {
            let mut sigs = vec![];
            let mut pks = vec![];
            let mut valid_ids = vec![];

            for &vid in validator_ids {
                let sig_key = SignatureKey {
                    validator_id: vid,
                    data_root: *data_root,
                };
                let Some(sig) = self.gossip_signatures.get(&sig_key) else {
                    continue;
                };
                let Ok(validator) = head_state.validators.get(vid) else {
                    continue;
                };
                sigs.push(sig.clone());
                pks.push(validator.pubkey.clone());
                valid_ids.push(vid);
            }

            if valid_ids.is_empty() {
                continue;
            }

            let participants = AggregationBits::from_validator_indices(&valid_ids);
            let Ok(proof) = AggregatedSignatureProof::aggregate(
                participants,
                pks,
                sigs,
                *data_root,
                attestation_data.slot.0 as u32,
            ) else {
                continue;
            };

            for vid in &valid_ids {
                let key = SignatureKey {
                    validator_id: *vid,
                    data_root: *data_root,
                };
                self.latest_new_aggregated_payloads
                    .entry(key.clone())
                    .or_default()
                    .push(proof.clone());
                to_remove.push(key);
            }

            new_aggregates.push(SignedAggregatedAttestation {
                data: attestation_data.clone(),
                proof,
            });
        }

        for key in to_remove {
            self.gossip_signatures.remove(&key);
        }

        new_aggregates
    }

    pub fn tick_interval(
        &mut self,
        has_proposal: bool,
        is_aggregator: bool,
    ) -> Vec<SignedAggregatedAttestation> {
        self.time += 1;
        let curr_interval = self.time % INTERVALS_PER_SLOT;
        let mut new_aggregates = vec![];

        match curr_interval {
            0 if has_proposal => self.accept_new_attestations(),
            2 if is_aggregator => new_aggregates = self.aggregate_committee_signatures(),
            3 => self.update_safe_target(),
            4 => self.accept_new_attestations(),
            _ => {}
        }

        new_aggregates
    }

    pub fn on_tick(
        &mut self,
        target_interval: u64,
        has_proposal: bool,
        is_aggregator: bool,
    ) -> Vec<SignedAggregatedAttestation> {
        let mut all_new_aggregates = vec![];

        while self.time < target_interval {
            let should_signal_proposal = has_proposal && (self.time + 1) == target_interval;
            let new_aggregates = self.tick_interval(should_signal_proposal, is_aggregator);
            all_new_aggregates.extend(new_aggregates);
        }

        all_new_aggregates
    }

    /// Algorithm:
    /// 1. Start at Head: Begin with the current head block
    /// 2. Walk Toward Safe: Move backward (up to JUSTIFICATION_LOOKBACK_SLOTS steps)
    ///    if safe target is newer
    /// 3. Ensure Justifiable: Continue walking back until slot is justifiable
    /// 4. Return Checkpoint: Create checkpoint from selected block
    pub fn get_attestation_target(&self) -> Checkpoint {
        let mut target = self.head;
        let safe_slot = self.blocks[&self.safe_target].slot;

        // Walk back toward safe target
        for _ in 0..JUSTIFICATION_LOOKBACK_SLOTS {
            if self.blocks[&target].slot > safe_slot {
                target = self.blocks[&target].parent_root;
            } else {
                break;
            }
        }

        let final_slot = self.latest_finalized.slot;
        while !self.blocks[&target].slot.is_justifiable_after(final_slot) {
            target = self.blocks[&target].parent_root;
        }

        let block_target = &self.blocks[&target];
        Checkpoint {
            root: target,
            slot: block_target.slot,
        }
    }

    /// Produce attestation data for the current slot.
    ///
    /// Returns AttestationData with:
    /// - slot: provided slot
    /// - head: current forkchoice head
    /// - target: checkpoint from get_attestation_target
    /// - source: current justified checkpoint
    pub fn produce_attestation_data(&self, slot: Slot) -> Result<AttestationData> {
        let head_root = self.head;

        let head_checkpoint = Checkpoint {
            root: head_root,
            slot: self
                .get_block(&head_root)
                .ok_or_else(|| anyhow!("Head block not found: {}", head_root))?
                .slot,
        };

        let target_checkpoint = self.get_attestation_target();

        Ok(AttestationData {
            slot,
            head: head_checkpoint,
            target: target_checkpoint,
            source: self.latest_justified.clone(),
        })
    }

    #[inline]
    pub fn get_proposal_head(&mut self, slot: Slot) -> H256 {
        // Advance time to this slot's first interval
        let target_interval = slot.0 * INTERVALS_PER_SLOT;
        self.on_tick(target_interval, true, false);
        self.accept_new_attestations();
        self.head
    }

    /// Produce a block and aggregated signature proofs for the target slot.
    ///
    /// # Algorithm Overview
    /// 1. **Get Proposal Head**: Retrieve current chain head as parent
    /// 2. **Verify Proposer**: Check validator is authorized for this slot
    /// 3. **Collect Attestations**: Extract from known aggregated payloads
    /// 4. **Build Block**: Use State.build_block with signature caches
    /// 5. **Store Block**: Insert block and post-state into Store
    ///
    /// # Arguments
    /// * `slot` - Target slot number for block production
    /// * `validator_index` - Index of validator authorized to propose this block
    ///
    /// # Returns
    /// Tuple of (block root, finalized Block, attestation signature proofs)
    pub fn produce_block_with_signatures(
        &mut self,
        slot: Slot,
        validator_index: u64,
    ) -> Result<(H256, Block, Vec<AggregatedSignatureProof>)> {
        // Get parent block head
        let head_root = self.get_proposal_head(slot);
        let head_state = self
            .states
            .get(&head_root)
            .ok_or_else(|| anyhow!("Head state not found"))?
            .clone();

        // Validate proposer authorization for this slot
        let num_validators = head_state.validators.len_u64();
        let expected_proposer = slot.0 % num_validators;
        ensure!(
            validator_index == expected_proposer,
            "Validator {} is not the proposer for slot {} (expected {})",
            validator_index,
            slot.0,
            expected_proposer
        );

        // Extract attestations from known aggregated payloads
        let known_payloads = self.latest_known_aggregated_payloads.clone();
        let attestation_data_map =
            self.extract_attestations_from_aggregated_payloads(&known_payloads);
        let available_attestations: Vec<Attestation> = attestation_data_map
            .into_iter()
            .map(|(validator_idx, attestation_data)| Attestation {
                validator_id: validator_idx,
                data: attestation_data,
            })
            .collect();

        // Get known block roots for attestation validation
        let known_block_roots: HashSet<H256> = self.blocks.keys().copied().collect();

        // Build block with fixed-point attestation collection and signature aggregation
        let (final_block, final_post_state, _aggregated_attestations, signatures) = head_state
            .build_block(
                slot,
                validator_index,
                head_root,
                None,
                Some(available_attestations),
                Some(&known_block_roots),
                Some(&self.gossip_signatures),
                Some(&self.latest_known_aggregated_payloads),
            )?;

        // Compute block root
        let block_root = final_block.hash_tree_root();

        // Update checkpoints from post-state
        let prev_finalized_slot = self.latest_finalized.slot;
        if final_post_state.latest_justified.slot > self.latest_justified.slot {
            self.latest_justified = final_post_state.latest_justified.clone();
        }
        if final_post_state.latest_finalized.slot > self.latest_finalized.slot {
            self.latest_finalized = final_post_state.latest_finalized.clone();
        }

        // Store block and state
        self.blocks.insert(block_root, final_block.clone());
        self.states.insert(block_root, final_post_state);

        // Prune stale attestation data when finalization advances
        if self.latest_finalized.slot > prev_finalized_slot {
            self.prune_stale_attestation_data();
        }

        Ok((block_root, final_block, signatures))
    }

    // ========== Block Processing ==========

    pub fn on_block(&mut self, signed_block: SignedBlockWithAttestation) -> Result<()> {
        let block = signed_block.message.block.clone();
        let block_root = block.hash_tree_root();

        // Skip duplicate blocks (idempotent operation)
        if self.blocks.contains_key(&block_root) {
            return Ok(());
        }

        // Verify parent chain is available.
        // If missing, the node must sync the parent chain first.
        let parent_root = block.parent_root;
        let parent_state = self
            .states
            .get(&parent_root)
            .ok_or_else(|| {
                anyhow!(
                    "Parent state not found (root={}). Sync parent chain before processing block at slot {}.",
                    parent_root,
                    block.slot.0
                )
            })?
            .clone();

        // Compute post-state via state transition function
        let post_state = parent_state.state_transition(signed_block.clone(), true)?;

        let prev_finalized_slot = self.latest_finalized.slot;

        // Update justified checkpoint if post-state has higher slot
        if post_state.latest_justified.slot > self.latest_justified.slot {
            self.latest_justified = post_state.latest_justified.clone();
        }

        // Update finalized checkpoint if post-state has higher slot
        if post_state.latest_finalized.slot > self.latest_finalized.slot {
            self.latest_finalized = post_state.latest_finalized.clone();
        }

        // Store block and state
        self.blocks.insert(block_root, block.clone());
        self.states.insert(block_root, post_state);

        // Process block body attestations and store their proofs in known payloads
        let aggregated_attestations = &block.body.attestations;
        let attestation_signatures = &signed_block.signature.attestation_signatures;

        ensure!(
            aggregated_attestations.len_u64() == attestation_signatures.len_u64(),
            "Attestation signature groups must match aggregated attestations"
        );

        for (att, proof) in aggregated_attestations
            .into_iter()
            .zip(attestation_signatures.into_iter())
        {
            let validator_ids = att.aggregation_bits.to_validator_indices();
            let data_root = att.data.hash_tree_root();

            // Store attestation data for later extraction
            self.attestation_data_by_root
                .insert(data_root, att.data.clone());

            for vid in &validator_ids {
                // Store proof in known payloads (block attestations are immediately known)
                let key = SignatureKey {
                    validator_id: *vid,
                    data_root,
                };
                self.latest_known_aggregated_payloads
                    .entry(key)
                    .or_default()
                    .push(proof.clone());
            }
        }

        // Store proposer attestation data
        let proposer_attestation = &signed_block.message.proposer_attestation;
        let proposer_data_root = proposer_attestation.data.hash_tree_root();
        self.attestation_data_by_root
            .insert(proposer_data_root, proposer_attestation.data.clone());

        // Update forkchoice head based on new block and attestations.
        // IMPORTANT: This must happen BEFORE processing proposer attestation
        // to prevent the proposer from gaining circular weight advantage.
        self.update_head();

        // Store proposer signature if it belongs to the same committee subnet
        if let Some(my_id) = self.validator_id {
            let proposer_subnet = proposer_attestation.validator_id % ATTESTATION_COMMITTEE_COUNT;
            let current_subnet = my_id % ATTESTATION_COMMITTEE_COUNT;

            if proposer_subnet == current_subnet {
                let proposer_sig_key = SignatureKey {
                    validator_id: proposer_attestation.validator_id,
                    data_root: proposer_data_root,
                };
                self.gossip_signatures.insert(
                    proposer_sig_key,
                    signed_block.signature.proposer_signature.clone(),
                );
            }
        }

        // Prune stale attestation data when finalization advances
        if self.latest_finalized.slot > prev_finalized_slot {
            self.prune_stale_attestation_data();
        }

        Ok(())
    }

    /// Check if a block with the given root exists in the store.
    #[inline]
    pub fn contains_block(&self, root: &H256) -> bool {
        self.blocks.contains_key(root)
    }

    /// Check if a state with the given root exists in the store.
    #[inline]
    pub fn contains_state(&self, root: &H256) -> bool {
        self.states.contains_key(root)
    }
}

#[cfg(test)]
impl Store {
    pub fn set_head(&mut self, head: H256) {
        self.head = head;
    }

    pub fn set_safe_target(&mut self, safe_target: H256) {
        self.safe_target = safe_target;
    }

    pub fn set_time(&mut self, time: Interval) {
        self.time = time;
    }

    pub fn insert_block(&mut self, root: H256, block: Block) {
        self.blocks.insert(root, block);
    }

    pub fn insert_state(&mut self, root: H256, state: State) {
        self.states.insert(root, state);
    }

    pub fn clear_known_attestations(&mut self) {
        self.latest_known_aggregated_payloads.clear();
    }

    pub fn insert_gossip_signature(&mut self, key: SignatureKey, sig: Signature) {
        self.gossip_signatures.insert(key, sig);
    }

    pub fn insert_aggregated_payload(
        &mut self,
        key: SignatureKey,
        proofs: Vec<AggregatedSignatureProof>,
    ) {
        self.latest_known_aggregated_payloads.insert(key, proofs);
    }

    pub fn set_latest_justified(&mut self, checkpoint: Checkpoint) {
        self.latest_justified = checkpoint;
    }

    pub fn set_latest_finalized(&mut self, checkpoint: Checkpoint) {
        self.latest_finalized = checkpoint;
    }

    pub fn blocks_mut(&mut self) -> &mut HashMap<H256, Block> {
        &mut self.blocks
    }

    pub fn states_mut(&mut self) -> &mut HashMap<H256, State> {
        &mut self.states
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use containers::{BlockBody, Validator};

    fn create_test_store() -> Store {
        let validators = vec![Validator::default(); 10];
        let state = State::generate_genesis_with_validators(1000, validators);

        let block = Block {
            slot: Slot(0),
            proposer_index: 0,
            parent_root: H256::default(),
            state_root: state.hash_tree_root(),
            body: BlockBody::default(),
        };

        Store::new(state, block, None)
    }

    fn create_attestation_data(
        slot: u64,
        head_root: H256,
        head_slot: u64,
        target_root: H256,
        target_slot: u64,
        source_root: H256,
        source_slot: u64,
    ) -> AttestationData {
        AttestationData {
            slot: Slot(slot),
            head: Checkpoint {
                root: head_root,
                slot: Slot(head_slot),
            },
            target: Checkpoint {
                root: target_root,
                slot: Slot(target_slot),
            },
            source: Checkpoint {
                root: source_root,
                slot: Slot(source_slot),
            },
        }
    }

    // Time tests
    #[test]
    fn test_on_tick_basic() {
        let mut store = create_test_store();
        let initial_time = store.time();
        let target_interval = 200u64;
        store.on_tick(target_interval, true, false);
        assert!(store.time() > initial_time);
    }

    #[test]
    fn test_on_tick_already_current() {
        let mut store = create_test_store();
        let initial_time = store.time();
        store.on_tick(initial_time, true, false);
        assert_eq!(store.time(), initial_time);
    }

    #[test]
    fn test_tick_interval_basic() {
        let mut store = create_test_store();
        let initial_time = store.time();
        store.tick_interval(false, false);
        assert_eq!(store.time(), initial_time + 1);
    }

    #[test]
    fn test_tick_interval_sequence() {
        let mut store = create_test_store();
        let initial_time = store.time();
        for i in 0..5 {
            store.tick_interval(i % 2 == 0, false);
        }
        assert_eq!(store.time(), initial_time + 5);
    }

    #[test]
    fn test_tick_interval_actions_by_phase() {
        let mut store = create_test_store();
        store.set_time(0);
        for interval in 0..INTERVALS_PER_SLOT {
            let has_proposal = interval == 0;
            store.tick_interval(has_proposal, false);
            let current_interval = store.time() % INTERVALS_PER_SLOT;
            let expected_interval = (interval + 1) % INTERVALS_PER_SLOT;
            assert_eq!(current_interval, expected_interval);
        }
    }

    // Fork choice tests
    #[test]
    fn test_get_vote_target_chain() {
        let mut store = create_test_store();
        let mut parent_root = store.head();

        for i in 1..=10 {
            let block = Block {
                slot: Slot(i),
                proposer_index: 0,
                parent_root,
                state_root: H256::default(),
                body: BlockBody::default(),
            };
            let block_root = block.hash_tree_root();
            store.insert_block(block_root, block);
            parent_root = block_root;
        }

        store.set_head(parent_root);
        let target = store.get_attestation_target();
        assert_eq!(target.slot, Slot(6));
    }

    // Vote tests
    #[test]
    fn test_competing_votes_different_blocks() {
        let mut store = create_test_store();
        let genesis_root = store.head();

        let block_a = Block {
            slot: Slot(1),
            proposer_index: 0,
            parent_root: genesis_root,
            state_root: H256::default(),
            body: BlockBody::default(),
        };
        let block_a_root = block_a.hash_tree_root();

        let mut block_b = block_a.clone();
        block_b.proposer_index = 1;
        let block_b_root = block_b.hash_tree_root();

        store.insert_block(block_a_root, block_a);
        store.insert_block(block_b_root, block_b);

        let mut attestations = HashMap::new();
        for i in 0..3 {
            attestations.insert(
                i,
                create_attestation_data(1, block_a_root, 1, genesis_root, 0, genesis_root, 0),
            );
        }
        for i in 3..5 {
            attestations.insert(
                i,
                create_attestation_data(1, block_b_root, 1, genesis_root, 0, genesis_root, 0),
            );
        }

        let head = store.get_fork_choice_head(genesis_root, &attestations, 0);
        assert_eq!(head, block_a_root);
    }

    #[test]
    fn test_vote_weight_accumulation() {
        let mut store = create_test_store();
        let genesis_root = store.head();

        let block1 = Block {
            slot: Slot(1),
            proposer_index: 0,
            parent_root: genesis_root,
            state_root: H256::default(),
            body: BlockBody::default(),
        };
        let block1_root = block1.hash_tree_root();

        let block2 = Block {
            slot: Slot(2),
            proposer_index: 0,
            parent_root: block1_root,
            state_root: H256::default(),
            body: BlockBody::default(),
        };
        let block2_root = block2.hash_tree_root();

        store.insert_block(block1_root, block1);
        store.insert_block(block2_root, block2);

        let mut attestations = HashMap::new();
        attestations.insert(
            0,
            create_attestation_data(2, block2_root, 2, genesis_root, 0, genesis_root, 0),
        );

        let head = store.get_fork_choice_head(genesis_root, &attestations, 0);
        assert_eq!(head, block2_root);
    }

    #[test]
    fn test_vote_for_unknown_block_ignored() {
        let store = create_test_store();
        let genesis_root = store.head();
        let unknown_root = H256::from_slice(&[0xff; 32]);

        let mut attestations = HashMap::new();
        attestations.insert(
            0,
            create_attestation_data(1, unknown_root, 1, genesis_root, 0, genesis_root, 0),
        );

        let head = store.get_fork_choice_head(genesis_root, &attestations, 0);
        assert_eq!(head, genesis_root);
    }

    // Validator/block production tests
    #[test]
    fn test_produce_block_basic() {
        let mut store = create_test_store();
        let initial_head = store.head();

        let (block_root, block, _) = store
            .produce_block_with_signatures(Slot(1), 1)
            .expect("block production should succeed");

        assert_eq!(block.slot, Slot(1));
        assert_eq!(block.proposer_index, 1);
        assert_eq!(block.parent_root, initial_head);
        assert_ne!(block.state_root, H256::default());
        assert!(store.contains_block(&block_root));
        assert!(store.contains_state(&block_root));
    }

    #[test]
    fn test_produce_block_unauthorized_proposer() {
        let mut store = create_test_store();
        let result = store.produce_block_with_signatures(Slot(1), 2);
        assert!(result.is_err());
        assert!(format!("{:?}", result.unwrap_err()).contains("is not the proposer for slot"));
    }

    #[test]
    fn test_produce_block_sequential_slots() {
        let mut store = create_test_store();

        let (block1_root, block1, _) = store
            .produce_block_with_signatures(Slot(1), 1)
            .expect("block1 should succeed");

        assert_eq!(block1.slot, Slot(1));
        assert!(store.contains_block(&block1_root));

        let (block2_root, block2, _) = store
            .produce_block_with_signatures(Slot(2), 2)
            .expect("block2 should succeed");

        assert_eq!(block2.slot, Slot(2));
        assert!(store.contains_block(&block2_root));
    }

    #[test]
    fn test_produce_block_empty_attestations() {
        let mut store = create_test_store();
        store.clear_known_attestations();

        let (_, block, _) = store
            .produce_block_with_signatures(Slot(3), 3)
            .expect("block production should succeed");

        assert_eq!(block.body.attestations.len_usize(), 0);
    }

    #[test]
    fn test_produce_block_missing_parent_state() {
        let validators = vec![Validator::default(); 3];
        let state = State::generate_genesis_with_validators(1000, validators);
        let genesis = Block {
            slot: Slot(0),
            proposer_index: 0,
            parent_root: H256::default(),
            state_root: state.hash_tree_root(),
            body: BlockBody::default(),
        };

        let mut store = Store::new(state, genesis, None);
        store.states_mut().clear();

        let result = store.produce_block_with_signatures(Slot(1), 1);
        assert!(result.is_err());
        assert!(format!("{:?}", result.unwrap_err()).contains("Head state not found"));
    }
}
