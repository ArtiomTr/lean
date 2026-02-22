use std::collections::HashMap;

use anyhow::{anyhow, ensure, Result};
use containers::{
    AggregatedAttestation, AggregatedSignatureProof, Attestation, AttestationData, Block,
    Checkpoint, Config, SignatureKey, SignedAggregatedAttestation, SignedAttestation,
    SignedBlockWithAttestation, Slot, State,
};
use metrics::set_gauge_u64;
use ssz::{SszHash, H256};
use xmss::Signature;

pub type Interval = u64;
pub const INTERVALS_PER_SLOT: Interval = 5;
pub const SECONDS_PER_SLOT: u64 = 5;
pub const SECONDS_PER_INTERVAL: u64 = SECONDS_PER_SLOT / INTERVALS_PER_SLOT;
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

    /// Queue for blocks awaiting parent blocks
    blocks_queue: HashMap<H256, Vec<SignedBlockWithAttestation>>,
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
    pub fn new(
        anchor_state: State,
        anchor_block: Block,
        validator_id: Option<u64>,
    ) -> Self {
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

        // Initialize checkpoints from the anchor state
        // We explicitly set the root to the anchor block root.
        // The anchor state internally might have zero-hash checkpoints (if genesis),
        // but the Store must treat the anchor block as the justified/finalized point.
        let latest_justified = Checkpoint {
            root: anchor_root,
            slot: anchor_state.latest_justified.slot,
        };
        let latest_finalized = Checkpoint {
            root: anchor_root,
            slot: anchor_state.latest_finalized.slot,
        };

        Store {
            time: anchor_slot.0 * INTERVALS_PER_SLOT,
            config: anchor_state.config.clone(),
            head: anchor_root,
            safe_target: anchor_root,
            latest_justified,
            latest_finalized,
            blocks: [(anchor_root, anchor_block)].into(),
            states: [(anchor_root, anchor_state)].into(),
            validator_id,
            gossip_signatures: HashMap::new(),
            attestation_data_by_root: HashMap::new(),
            latest_new_aggregated_payloads: HashMap::new(),
            latest_known_aggregated_payloads: HashMap::new(),
            blocks_queue: HashMap::new(),
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
    pub fn latest_known_attestations(&self) -> &HashMap<u64, AttestationData> {
        &self.latest_known_attestations
    }

    #[inline]
    pub fn latest_new_attestations(&self) -> &HashMap<u64, AttestationData> {
        &self.latest_new_attestations
    }

    #[inline]
    pub fn blocks_queue(&self) -> &HashMap<H256, Vec<SignedBlockWithAttestation>> {
        &self.blocks_queue
    }

    #[inline]
    pub fn gossip_signatures(&self) -> &HashMap<SignatureKey, Signature> {
        &self.gossip_signatures
    }

    #[inline]
    pub fn aggregated_payloads(&self) -> &HashMap<SignatureKey, Vec<AggregatedSignatureProof>> {
        &self.aggregated_payloads
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
        let stale_roots: Vec<H256> = self
            .attestation_data_by_root
            .iter()
            .filter(|(_, data)| data.target.slot <= finalized_slot)
            .map(|(root, _)| *root)
            .collect();

        if stale_roots.is_empty() {
            return;
        }

        // Remove stale entries from all attestation-related maps
        for root in &stale_roots {
            self.attestation_data_by_root.remove(root);
        }

        self.gossip_signatures
            .retain(|key, _| !stale_roots.contains(&key.data_root));

        self.latest_new_aggregated_payloads
            .retain(|key, _| !stale_roots.contains(&key.data_root));

        self.latest_known_aggregated_payloads
            .retain(|key, _| !stale_roots.contains(&key.data_root));
    }

    /// Check if ancestor is an ancestor of descendant in the block tree.
    ///
    /// Walks backwards from descendant to see if we reach ancestor.
    fn is_ancestor(&self, ancestor: H256, descendant: H256) -> Result<bool> {
        if ancestor == descendant {
            return Ok(true);
        }

        let mut current = descendant;
        loop {
            let block = self
                .get_block(&current)
                .ok_or_else(|| anyhow!("Block not found: {}", current))?;

            if block.parent_root == ancestor {
                return Ok(true);
            }

            if block.parent_root.is_zero() {
                return Ok(false);
            }

            current = block.parent_root;

            // Prevent infinite loops in case of circular references
            if current == descendant {
                return Ok(false);
            }
        }
    }

    /// Validate an attestation according to fork choice rules.
    ///
    /// Four validation checks:
    /// 1. Slot boundary: attestation.data.slot + 1 < current_slot
    /// 2. Head ancestry: attestation.data.head is ancestor of attestation.data.target
    /// 3. Source checkpoint: target state's justified matches attestation source
    /// 4. Target slot: attestation.data.target.slot == attestation.data.slot
    pub fn validate_attestation(&self, attestation: &Attestation) -> Result<()> {
        let current_slot = self.time / INTERVALS_PER_SLOT;

        // Check 1: Slot boundary
        ensure!(
            attestation.data.slot.0 + 1 < current_slot,
            "Attestation too recent: slot {} must be < current {}",
            attestation.data.slot.0,
            current_slot
        );

        // Check 2: Head ancestry (head must be ancestor of target)
        ensure!(
            self.is_ancestor(attestation.data.head.root, attestation.data.target.root)?,
            "Head {} not ancestor of target {}",
            attestation.data.head.root,
            attestation.data.target.root
        );

        // Check 3: Source checkpoint (must match target state's justified)
        let target_state = self
            .get_state(&attestation.data.target.root)
            .ok_or_else(|| anyhow!("Target state not found: {}", attestation.data.target.root))?;

        ensure!(
            target_state.latest_justified == attestation.data.source,
            "Invalid source checkpoint"
        );

        // Check 4: Target slot (must match attestation slot)
        ensure!(
            attestation.data.target.slot == attestation.data.slot,
            "Target slot mismatch: {} != {}",
            attestation.data.target.slot.0,
            attestation.data.slot.0
        );

        Ok(())
    }

    /// Get attestation target for a given head and slot.
    ///
    /// Walks backward from head until finding the block at the target slot.
    /// Returns (target_root, target_slot).
    fn get_attestation_target(&self, head: H256, slot: Slot) -> Result<(H256, Slot)> {
        let target_slot = slot; // In Lean, target slot equals attestation slot
        let mut current = head;

        loop {
            let block = self
                .get_block(&current)
                .ok_or_else(|| anyhow!("Block not found: {}", current))?;

            if block.slot == target_slot {
                return Ok((current, target_slot));
            }

            ensure!(
                block.slot < target_slot,
                "Target slot {} not found walking back from head {}",
                target_slot.0,
                head
            );

            current = block.parent_root;
        }
    }

    /// Produce attestation data for the current slot.
    ///
    /// Returns AttestationData with:
    /// - slot: current slot
    /// - head: current forkchoice head
    /// - target: checkpoint at attestation slot
    /// - source: current justified checkpoint from head state
    pub fn produce_attestation_data(&self, slot: Slot) -> Result<AttestationData> {
        let head_root = self.head;
        let (target_root, target_slot) = self.get_attestation_target(head_root, slot)?;

        let head_state = self
            .get_state(&head_root)
            .ok_or_else(|| anyhow!("Head state not found: {}", head_root))?;

        Ok(AttestationData {
            slot,
            head: Checkpoint {
                root: head_root,
                slot: self
                    .get_block(&head_root)
                    .ok_or_else(|| anyhow!("Head block not found: {}", head_root))?
                    .slot,
            },
            target: Checkpoint {
                root: target_root,
                slot: target_slot,
            },
            source: head_state.latest_justified.clone(),
        })
    }

    // ========== Gossip Attestation Handling ==========

    /// Process a signed attestation received via gossip network.
    ///
    /// This method:
    /// 1. Validates the attestation
    /// 2. Verifies the XMSS signature
    /// 3. If current node is aggregator, stores the signature (with subnet filtering)
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

        // Validate attestation
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

        let public_key = &target_state.validators[validator_id as usize].public_key;

        // Verify signature
        let data_root = attestation_data.hash_tree_root();
        signature
            .verify(public_key, attestation_data.slot.0 as u32, data_root)
            .map_err(|e| anyhow!("Signature verification failed: {}", e))?;

        // Store signature if aggregator with subnet filtering
        if is_aggregator {
            if let Some(my_id) = self.validator_id {
                // Compute subnets (simple modulo for now - can be made more sophisticated)
                const ATTESTATION_COMMITTEE_COUNT: u64 = 64;
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
        }

        // Store attestation data for later extraction
        self.attestation_data_by_root
            .insert(data_root, attestation_data.clone());

        Ok(())
    }

    /// Process a signed aggregated attestation received via aggregation topic.
    ///
    /// This method:
    /// 1. Validates the attestation data
    /// 2. Verifies the aggregated XMSS proof
    /// 3. Stores the aggregation in latest_new_aggregated_payloads
    ///
    /// # Arguments
    /// * `signed_attestation` - The signed aggregated attestation from committee aggregation
    pub fn on_gossip_aggregated_attestation(
        &mut self,
        signed_attestation: &SignedAggregatedAttestation,
    ) -> Result<()> {
        let attestation_data = &signed_attestation.message.data;
        let aggregation_bits = &signed_attestation.message.aggregation_bits;
        let signature = &signed_attestation.signature;

        // Validate attestation data
        let attestation = Attestation {
            validator_id: 0, // Dummy, not used for aggregated
            data: attestation_data.clone(),
        };
        self.validate_attestation(&attestation)?;

        // Get validator IDs who participated
        let validator_ids = aggregation_bits.to_validator_indices();

        // Get state for verification
        let target_state = self.get_state(&attestation_data.target.root).ok_or_else(|| {
            anyhow!(
                "No state available to verify committee aggregation for target {}",
                attestation_data.target.root
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
            .map(|&vid| target_state.validators[vid as usize].public_key.clone())
            .collect();

        // Verify the aggregated proof
        let data_root = attestation_data.hash_tree_root();
        let proof = AggregatedSignatureProof {
            participants: aggregation_bits.clone(),
            proof_data: signature.clone(),
        };

        proof
            .verify(public_keys, data_root, attestation_data.slot.0 as u32)
            .map_err(|e| anyhow!("Committee aggregation signature verification failed: {}", e))?;

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

        // Store attestation data
        self.attestation_data_by_root
            .insert(data_root, attestation_data.clone());

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
        let mut vote_weights: HashMap<H256, usize> = HashMap::new();
        let root_slot = self.blocks[&root].slot;

        // stage 1: accumulate weights by walking up from each attestation's head
        for attestation_data in latest_attestations.values() {
            let mut curr = attestation_data.head.root;

            if let Some(block) = self.blocks.get(&curr) {
                let mut curr_slot = block.slot;

                while curr_slot > root_slot {
                    *vote_weights.entry(curr).or_insert(0) += 1;

                    if let Some(parent_block) = self.blocks.get(&curr) {
                        curr = parent_block.parent_root;
                        if curr.is_zero() {
                            break;
                        }
                        if let Some(next_block) = self.blocks.get(&curr) {
                            curr_slot = next_block.slot;
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            }
        }

        // stage 2: build adjacency tree (parent -> children)
        let mut child_map: HashMap<H256, Vec<H256>> = HashMap::new();
        for (block_hash, block) in &self.blocks {
            if !block.parent_root.is_zero() {
                if vote_weights.get(block_hash).copied().unwrap_or(0) >= min_votes {
                    child_map
                        .entry(block.parent_root)
                        .or_default()
                        .push(*block_hash);
                }
            }
        }

        // stage 3: greedy walk choosing heaviest child at each fork
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

    pub fn get_latest_justified(states: &HashMap<H256, State>) -> Option<&Checkpoint> {
        states
            .values()
            .map(|state| &state.latest_justified)
            .max_by_key(|checkpoint| checkpoint.slot)
    }

    pub fn update_head(&mut self) {
        // Compute new head using LMD-GHOST from latest justified root
        let new_head = self.get_fork_choice_head(
            self.latest_justified.root,
            &self.latest_known_attestations,
            0,
        );
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

        let min_score = (n_validators * 2 + 2) / 3;
        let root = self.latest_justified.root;
        let new_safe_target =
            self.get_fork_choice_head(root, &self.latest_new_attestations, min_score);
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
        // Drain latest_new_attestations into latest_known_attestations
        let new_attestations: Vec<_> = self.latest_new_attestations.drain().collect();
        for (k, v) in new_attestations {
            self.latest_known_attestations.insert(k, v);
        }
        self.update_head();
    }

    pub fn tick_interval(&mut self, has_proposal: bool) {
        self.time += 1;
        // Calculate current interval within slot: time % INTERVALS_PER_SLOT
        let curr_interval = self.time % INTERVALS_PER_SLOT;

        match curr_interval {
            0 if has_proposal => self.accept_new_attestations(),
            2 => self.update_safe_target(),
            3 => self.accept_new_attestations(),
            _ => {}
        }
    }

    /// Algorithm:
    /// 1. Start at Head: Begin with the current head block
    /// 2. Walk Toward Safe: Move backward (up to JUSTIFICATION_LOOKBACK_SLOTS steps)
    ///    if safe target is newer
    /// 3. Ensure Justifiable: Continue walking back until slot is justifiable
    /// 4. Return Checkpoint: Create checkpoint from selected block
    pub fn get_vote_target(&self) -> Checkpoint {
        let mut target = self.head;
        let safe_slot = self.blocks[&self.safe_target].slot;

        // Walk back toward safe target
        for _ in 0..3 {
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

    #[inline]
    pub fn get_proposal_head(&mut self, slot: Slot) -> H256 {
        let slot_time = self.config.genesis_time + (slot.0 * SECONDS_PER_SLOT);

        self.on_tick(slot_time, true);
        self.accept_new_attestations();
        self.head
    }

    #[inline]
    pub fn on_tick(&mut self, time: u64, has_proposal: bool) {
        // Calculate target time in intervals
        let tick_interval_time =
            time.saturating_sub(self.config.genesis_time) / SECONDS_PER_INTERVAL;

        // Tick forward one interval at a time
        while self.time < tick_interval_time {
            // Check if proposal should be signaled for next interval
            let should_signal_proposal = has_proposal && (self.time + 1) == tick_interval_time;

            // Advance by one interval with appropriate signaling
            self.tick_interval(should_signal_proposal);
        }
    }

    /// Produce a block and aggregated signature proofs for the target slot per devnet-2.
    ///
    /// The proposer returns the block and `MultisigAggregatedSignature` proofs aligned
    /// with `block.body.attestations` so it can craft `SignedBlockWithAttestation`.
    ///
    /// # Algorithm Overview
    /// 1. **Get Proposal Head**: Retrieve current chain head as parent
    /// 2. **Collect Attestations**: Convert known attestations to plain attestations
    /// 3. **Build Block**: Use State.build_block with signature caches
    /// 4. **Store Block**: Insert block and post-state into Store
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

        // Convert AttestationData to Attestation objects for build_block
        // Per devnet-2, store now holds AttestationData directly
        let available_attestations: Vec<Attestation> = self
            .latest_known_attestations
            .iter()
            .map(|(validator_idx, attestation_data)| Attestation {
                validator_id: *validator_idx,
                data: attestation_data.clone(),
            })
            .collect();

        // Get known block roots for attestation validation
        let known_block_roots: std::collections::HashSet<H256> =
            self.blocks.keys().copied().collect();

        // Build block with fixed-point attestation collection and signature aggregation
        let (final_block, final_post_state, _aggregated_attestations, signatures) = head_state
            .build_block(
                slot,
                validator_index,
                head_root,
                None, // initial_attestations - start with empty, let fixed-point collect
                Some(available_attestations),
                Some(&known_block_roots),
                Some(&self.gossip_signatures),
                Some(&self.aggregated_payloads),
            )?;

        // Compute block root using the header hash (canonical block root)
        let block_root = final_block.hash_tree_root();

        // Store block and state (per devnet-2, we store the plain Block)
        self.blocks.insert(block_root, final_block.clone());
        self.states.insert(block_root, final_post_state);

        Ok((block_root, final_block, signatures))
    }

    // ========== Block and Attestation Processing ==========

    /// Process a block and add it to the fork choice store.
    ///
    /// Algorithm:
    /// 1. Skip duplicate blocks (idempotent operation)
    /// 2. Check if parent exists; queue block if not
    /// 3. Compute post-state via state transition
    /// 4. Update justified/finalized checkpoints if needed
    /// 5. Process queued children blocks
    // ========== Block Processing ==========

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
            // Get attestation data from the data root
            let data_root = sig_key.data_root;
            let Some(attestation_data) = self.attestation_data_by_root.get(&data_root) else {
                continue; // Skip if we don't have the attestation data
            };

            // Extract validator IDs from all proofs
            for proof in proofs {
                let validator_ids = proof.get_participant_indices();
                for vid in validator_ids {
                    // Store attestation data for this validator
                    // If multiple attestations exist for same validator, keep the latest
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

    pub fn on_block(&mut self, signed_block: SignedBlockWithAttestation) -> Result<()> {
        let block = signed_block.message.block.clone();
        let block_root = block.hash_tree_root();

        // Skip duplicate blocks
        if self.blocks.contains_key(&block_root) {
            return Ok(());
        }

        // Check if parent exists
        let parent_root = block.parent_root;
        if !self.blocks.contains_key(&parent_root) && !parent_root.is_zero() {
            // Queue block for later processing
            self.blocks_queue
                .entry(parent_root)
                .or_default()
                .push(signed_block);
            return Err(anyhow!("Block queued: parent not found"));
        }

        // Get parent state
        let parent_state = self
            .states
            .get(&parent_root)
            .ok_or_else(|| anyhow!("Parent state not found"))?
            .clone();

        // Compute post-state via state transition
        // valid_signatures = true (assume signatures are already verified)
        let post_state = parent_state.state_transition(signed_block, true)?;

        // Update justified checkpoint if post-state has higher slot
        if post_state.latest_justified.slot > self.latest_justified.slot {
            self.latest_justified = post_state.latest_justified.clone();
        }

        // Update finalized checkpoint if post-state has higher slot
        if post_state.latest_finalized.slot > self.latest_finalized.slot {
            self.latest_finalized = post_state.latest_finalized.clone();
        }

        // Store block and state
        self.blocks.insert(block_root, block);
        self.states.insert(block_root, post_state);

        // Update head
        self.update_head();

        // Process queued children
        if let Some(queued) = self.blocks_queue.remove(&block_root) {
            for child_block in queued {
                // Ignore errors from queued blocks
                let _ = self.on_block(child_block);
            }
        }

        Ok(())
    }

    /// Process an attestation and add it to the fork choice store.
    ///
    /// If `is_proposer` is true, the attestation goes directly to known attestations.
    /// Otherwise, it goes to new attestations (pending until interval 3).
    pub fn on_attestation(
        &mut self,
        attestation: SignedAttestation,
        is_proposer: bool,
    ) -> Result<()> {
        let validator_id = attestation.validator_id;
        let data = attestation.message;

        // Validate that the attestation's head block exists
        if !self.blocks.contains_key(&data.head.root) {
            return Err(anyhow!("Attestation head block not found"));
        }

        if is_proposer {
            self.latest_known_attestations.insert(validator_id, data);
        } else {
            self.latest_new_attestations.insert(validator_id, data);
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

    pub fn insert_latest_known_attestation(&mut self, validator_id: u64, data: AttestationData) {
        self.latest_known_attestations.insert(validator_id, data);
    }

    /// Alias for insert_latest_known_attestation (used by tests)
    pub fn insert_known_attestation(&mut self, validator_id: u64, data: AttestationData) {
        self.latest_known_attestations.insert(validator_id, data);
    }

    pub fn insert_latest_new_attestation(&mut self, validator_id: u64, data: AttestationData) {
        self.latest_new_attestations.insert(validator_id, data);
    }

    pub fn clear_known_attestations(&mut self) {
        self.latest_known_attestations.clear();
    }

    pub fn insert_gossip_signature(&mut self, key: SignatureKey, sig: Signature) {
        self.gossip_signatures.insert(key, sig);
    }

    pub fn insert_aggregated_payload(&mut self, key: SignatureKey, proofs: Vec<AggregatedSignatureProof>) {
        self.aggregated_payloads.insert(key, proofs);
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
    use containers::{BlockBody, BlockWithAttestation, Validator};

    fn create_test_store() -> Store {
        let config = Config { genesis_time: 1000 };
        let validators = vec![Validator::default(); 10];
        let state = State::generate_genesis_with_validators(1000, validators);

        let block = Block {
            slot: Slot(0),
            proposer_index: 0,
            parent_root: H256::default(),
            state_root: state.hash_tree_root(),
            body: BlockBody::default(),
        };

        let block_with_attestation = BlockWithAttestation {
            block,
            proposer_attestation: Attestation::default(),
        };

        let signed_block = SignedBlockWithAttestation {
            message: block_with_attestation,
            signature: Default::default(),
        };

        Store::new(state, signed_block, config)
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
            head: Checkpoint { root: head_root, slot: Slot(head_slot) },
            target: Checkpoint { root: target_root, slot: Slot(target_slot) },
            source: Checkpoint { root: source_root, slot: Slot(source_slot) },
        }
    }

    fn produce_attestation_data(store: &Store, slot: Slot) -> AttestationData {
        let head_block = store.blocks().get(&store.head()).unwrap();
        let head_checkpoint = Checkpoint {
            root: store.head(),
            slot: head_block.slot,
        };
        AttestationData {
            slot,
            head: head_checkpoint,
            target: store.get_vote_target(),
            source: store.latest_justified().clone(),
        }
    }

    // Time tests
    #[test]
    fn test_on_tick_basic() {
        let mut store = create_test_store();
        let initial_time = store.time();
        let target_time = store.config().genesis_time + 200;
        store.on_tick(target_time, true);
        assert!(store.time() > initial_time);
    }

    #[test]
    fn test_on_tick_already_current() {
        let mut store = create_test_store();
        let initial_time = store.time();
        let current_target = store.config().genesis_time + initial_time;
        store.on_tick(current_target, true);
        assert_eq!(store.time(), initial_time);
    }

    #[test]
    fn test_tick_interval_basic() {
        let mut store = create_test_store();
        let initial_time = store.time();
        store.tick_interval(false);
        assert_eq!(store.time(), initial_time + 1);
    }

    #[test]
    fn test_tick_interval_sequence() {
        let mut store = create_test_store();
        let initial_time = store.time();
        for i in 0..5 {
            store.tick_interval(i % 2 == 0);
        }
        assert_eq!(store.time(), initial_time + 5);
    }

    #[test]
    fn test_tick_interval_actions_by_phase() {
        let mut store = create_test_store();
        store.set_time(0);
        for interval in 0..INTERVALS_PER_SLOT {
            let has_proposal = interval == 0;
            store.tick_interval(has_proposal);
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
        let target = store.get_vote_target();
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
            attestations.insert(i, create_attestation_data(1, block_a_root, 1, genesis_root, 0, genesis_root, 0));
        }
        for i in 3..5 {
            attestations.insert(i, create_attestation_data(1, block_b_root, 1, genesis_root, 0, genesis_root, 0));
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
        attestations.insert(0, create_attestation_data(2, block2_root, 2, genesis_root, 0, genesis_root, 0));

        let head = store.get_fork_choice_head(genesis_root, &attestations, 0);
        assert_eq!(head, block2_root);
    }

    #[test]
    fn test_vote_for_unknown_block_ignored() {
        let store = create_test_store();
        let genesis_root = store.head();
        let unknown_root = H256::from_slice(&[0xff; 32]);

        let mut attestations = HashMap::new();
        attestations.insert(0, create_attestation_data(1, unknown_root, 1, genesis_root, 0, genesis_root, 0));

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
    fn test_block_production_then_attestation() {
        let mut store = create_test_store();

        let _ = store
            .produce_block_with_signatures(Slot(1), 1)
            .expect("block should succeed");

        store.update_head();

        let attestation_data = produce_attestation_data(&store, Slot(2));
        let attestation = Attestation {
            validator_id: 7,
            data: attestation_data,
        };

        assert_eq!(attestation.validator_id, 7);
        assert_eq!(attestation.data.slot, Slot(2));
        assert_eq!(attestation.data.source, *store.latest_justified());
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
        let block_with_attestation = BlockWithAttestation {
            block: genesis,
            proposer_attestation: Attestation::default(),
        };
        let signed_block = SignedBlockWithAttestation {
            message: block_with_attestation,
            signature: Default::default(),
        };

        let mut store = Store::new(state, signed_block, Config { genesis_time: 1000 });
        store.states_mut().clear();

        let result = store.produce_block_with_signatures(Slot(1), 1);
        assert!(result.is_err());
        assert!(format!("{:?}", result.unwrap_err()).contains("Head state not found"));
    }
}
