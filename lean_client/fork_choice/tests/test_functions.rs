use ssz::H256;

use containers::{
    block::{hash_tree_root, Block, SignedBlock},
    checkpoint::Checkpoint,
    Config, Root, Slot, State, ValidatorIndex,
};

pub fn build_test_block(slot: u64, parent_root: Root, txt: &str) -> (Root, SignedBlock) {
    build_test_block_with_proposer(slot, parent_root, txt, ValidatorIndex(0))
}

pub fn build_test_block_with_proposer(
    slot: u64,
    parent_root: Root,
    txt: &str,
    proposer: ValidatorIndex,
) -> (Root, SignedBlock) {
    let mut root_bytes_state = [0u8; 32];

    let txt_bytes = txt.as_bytes();
    let len = std::cmp::min(txt_bytes.len(), 32);
    root_bytes_state[..len].copy_from_slice(&txt_bytes[..len]);

    let b = Block {
        slot: Slot(slot),
        parent_root,
        state_root: Root(H256::from_slice(&root_bytes_state)),
        proposer_index: proposer,
        ..Default::default()
    };

    let root = hash_tree_root(&b);
    let s_block = SignedBlock {
        message: b,
        ..Default::default()
    };
    (root, s_block)
}

pub fn build_checkpoint(root: Root, slot: u64) -> Checkpoint {
    Checkpoint {
        root,
        slot: Slot(slot),
    }
}

#[allow(dead_code)]
pub fn config_with_validators(num_validators: u64) -> Config {
    Config {
        num_validators,
        genesis_time: 0,
    }
}

pub fn build_valid_test_block(
    slot: u64,
    parent_root: Root,
    parent_state: &State,
    proposer: ValidatorIndex,
    txt: &str,
) -> (Root, SignedBlock) {
    use containers::block::{Block, BlockBody, SignedBlock};
    use containers::Slot;

    let mut root_bytes = [0u8; 32];
    let txt_bytes = txt.as_bytes();
    let len = std::cmp::min(txt_bytes.len(), 32);
    root_bytes[..len].copy_from_slice(&txt_bytes[..len]);

    let temp_block = Block {
        slot: Slot(slot),
        parent_root,
        proposer_index: proposer,
        state_root: Root(H256::from_slice(&root_bytes)),
        body: BlockBody::default(),
    };

    let new_state = parent_state.process_slots(Slot(slot));
    let new_state = new_state.process_block(&temp_block);
    let correct_state_root = hash_tree_root(&new_state);

    let final_block = Block {
        slot: Slot(slot),
        parent_root,
        proposer_index: proposer,
        state_root: correct_state_root,
        body: BlockBody::default(),
    };

    let root = hash_tree_root(&final_block);
    let signed_block = SignedBlock {
        message: final_block,
        ..Default::default()
    };

    (root, signed_block)
}
