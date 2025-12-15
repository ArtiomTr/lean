// DEVNET 0 validator clientas
use std::collections::HashMap;
use std::path::Path;

use containers::{
    attestation::{Attestation, AttestationData, BlockSignatures, Signature, SignedAttestation},
    block::{BlockWithAttestation, SignedBlockWithAttestation},
    checkpoint::Checkpoint,
    types::{Uint64, ValidatorIndex},
    Slot,
};
use fork_choice::store::{get_proposal_head, get_vote_target, Store};
use tracing::{info, warn};

pub type ValidatorRegistry = HashMap<String, Vec<u64>>;
// Node
#[derive(Debug, Clone)]
pub struct ValidatorConfig {
    pub node_id: String,
    pub validator_indices: Vec<u64>,
}

impl ValidatorConfig {
    // load validator index
    pub fn load_from_file(
        path: impl AsRef<Path>,
        node_id: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let file = std::fs::File::open(path)?;
        let registry: ValidatorRegistry = serde_yaml::from_reader(file)?;

        let indices = registry
            .get(node_id)
            .ok_or_else(|| format!("Node '{}' not found in validator registry", node_id))?
            .clone();

        info!(node_id = %node_id, indices = ?indices, "Validator config loaded...");

        Ok(ValidatorConfig {
            node_id: node_id.to_string(),
            validator_indices: indices,
        })
    }

    pub fn is_assigned(&self, index: u64) -> bool {
        self.validator_indices.contains(&index)
    }
}

pub struct ValidatorService {
    pub config: ValidatorConfig,
    pub num_validators: u64,
}

impl ValidatorService {
    pub fn new(config: ValidatorConfig, num_validators: u64) -> Self {
        info!(
            node_id = %config.node_id,
            indices = ?config.validator_indices,
            total_validators = num_validators,
            "VALIDATOR INITIALIZED SUCCESSFULLY"
        );
        Self {
            config,
            num_validators,
        }
    }

    pub fn get_proposer_for_slot(&self, slot: Slot) -> Option<ValidatorIndex> {
        if self.num_validators == 0 {
            return None;
        }
        let proposer = slot.0 % self.num_validators;

        if self.config.is_assigned(proposer) {
            Some(ValidatorIndex(proposer))
        } else {
            None
        }
    }

    /// Build a block proposal for the given slot
    /// Uses ZERO signatures for Devnet 0
    pub fn build_block_proposal(
        &self,
        store: &mut Store,
        slot: Slot,
        proposer_index: ValidatorIndex,
    ) -> Result<SignedBlockWithAttestation, String> {
        info!(
            slot = slot.0,
            proposer = proposer_index.0,
            "Building block proposal"
        );

        let parent_root = get_proposal_head(store, slot);
        let parent_state = store
            .states
            .get(&parent_root)
            .ok_or_else(|| format!("Couldn't find parent state {:?}", parent_root))?;

        let vote_target = get_vote_target(store);

        let head_block = store
            .blocks
            .get(&store.head)
            .ok_or("Head block not found")?;
        let head_checkpoint = Checkpoint {
            root: store.head,
            slot: head_block.message.block.slot,
        };

        let proposer_attestation = Attestation {
            validator_id: Uint64(proposer_index.0),
            data: AttestationData {
                slot,
                head: head_checkpoint,
                target: vote_target.clone(),
                source: store.latest_justified.clone(),
            },
        };

        // STATELESS BUILD?
        let (block, _post_state, _collected_atts, _sigs) =
            parent_state.build_block(slot, proposer_index, parent_root, None, None, None)?;

        info!(
            slot = block.slot.0,
            proposer = block.proposer_index.0,
            parent_root = %format!("0x{:x}", block.parent_root.0),
            state_root = %format!("0x{:x}", block.state_root.0),
            "Block built successfully"
        );

        let signed_block = SignedBlockWithAttestation {
            message: BlockWithAttestation {
                block,
                proposer_attestation,
            },
            signature: BlockSignatures::default(),
        };

        Ok(signed_block)
    }

    /// Create attestations for all our validators for the given slot
    /// Uses ZERO signatures for Devnet 0
    pub fn create_attestations(&self, store: &Store, slot: Slot) -> Vec<SignedAttestation> {
        let vote_target = get_vote_target(store);

        let get_head_block_info = match store.blocks.get(&store.head) {
            Some(b) => b,
            None => {
                // Pasileiskit, su DEBUG. Kitaip galima pakeist i tiesiog
                // println!("WARNNING: Attestation skipped. (Reason: HEAD BLOCK NOT FOUND)\n");
                warn!("WARNNING: Attestation skipped. (Reason: HEAD BLOCK NOT FOUND)");
                return vec![];
            }
        };

        let head_checkpoint = Checkpoint {
            root: store.head,
            slot: get_head_block_info.message.block.slot,
        };

        self.config
            .validator_indices
            .iter()
            .map(|&idx| {
                let attestation = Attestation {
                    validator_id: Uint64(idx),
                    data: AttestationData {
                        slot,
                        head: head_checkpoint.clone(),
                        target: vote_target.clone(),
                        source: store.latest_justified.clone(),
                    },
                };

                info!(
                    slot = slot.0,
                    validator = idx,
                    target_slot = vote_target.slot.0,
                    source_slot = store.latest_justified.slot.0,
                    "Created attestation"
                );

                // Devnet 0 with 0 signature
                SignedAttestation {
                    message: attestation,
                    signature: Signature::default(),
                }
            })
            .collect()
    }
}

// DI GENERUOTI TESTAI
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proposer_selection() {
        let config = ValidatorConfig {
            node_id: "test_0".to_string(),
            validator_indices: vec![2],
        };
        let service = ValidatorService::new(config, 4);

        // Validator 2 should propose at slots 2, 6, 10, ...
        assert!(service.get_proposer_for_slot(Slot(2)).is_some());
        assert!(service.get_proposer_for_slot(Slot(6)).is_some());
        assert!(service.get_proposer_for_slot(Slot(10)).is_some());

        // Validator 2 should NOT propose at slots 0, 1, 3, 4, 5, ...
        assert!(service.get_proposer_for_slot(Slot(0)).is_none());
        assert!(service.get_proposer_for_slot(Slot(1)).is_none());
        assert!(service.get_proposer_for_slot(Slot(3)).is_none());
        assert!(service.get_proposer_for_slot(Slot(4)).is_none());
        assert!(service.get_proposer_for_slot(Slot(5)).is_none());
    }

    #[test]
    fn test_is_assigned() {
        let config = ValidatorConfig {
            node_id: "test_0".to_string(),
            validator_indices: vec![2, 5, 8],
        };

        assert!(config.is_assigned(2));
        assert!(config.is_assigned(5));
        assert!(config.is_assigned(8));
        assert!(!config.is_assigned(0));
        assert!(!config.is_assigned(1));
        assert!(!config.is_assigned(3));
    }
}
