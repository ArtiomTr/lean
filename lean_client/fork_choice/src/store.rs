use containers::{
    block::SignedBlockWithAttestation, checkpoint::Checkpoint, config::Config, state::State,
    Bytes32, Root, Slot, ValidatorIndex,
};
use ssz::SszHash;
use std::collections::HashMap;
pub type Interval = u64;
pub const INTERVALS_PER_SLOT: Interval = 4;
pub const SECONDS_PER_SLOT: u64 = 4;

#[derive(Debug, Clone, Default)]
pub struct Store {
    pub time: Interval,
    pub config: Config,
    pub head: Root,
    pub safe_target: Root,
    pub latest_justified: Checkpoint,
    pub latest_finalized: Checkpoint,
    pub blocks: HashMap<Root, SignedBlockWithAttestation>,
    pub states: HashMap<Root, State>,
    pub latest_known_votes: HashMap<ValidatorIndex, Checkpoint>,
    pub latest_new_votes: HashMap<ValidatorIndex, Checkpoint>,
    pub blocks_queue: HashMap<Root, Vec<SignedBlockWithAttestation>>,
}

pub fn get_forkchoice_store(
    anchor_state: State,
    anchor_block: SignedBlockWithAttestation,
    config: Config,
) -> Store {
    let block_root = Bytes32(anchor_block.message.block.hash_tree_root());
    let block_slot = anchor_block.message.block.slot;

    let latest_justified = if anchor_state.latest_justified.root.0.is_zero() {
        Checkpoint {
            root: block_root,
            slot: block_slot,
        }
    } else {
        anchor_state.latest_justified.clone()
    };

    let latest_finalized = if anchor_state.latest_finalized.root.0.is_zero() {
        Checkpoint {
            root: block_root,
            slot: block_slot,
        }
    } else {
        anchor_state.latest_finalized.clone()
    };

    Store {
        time: block_slot.0 * INTERVALS_PER_SLOT,
        config,
        head: block_root,
        safe_target: block_root,
        latest_justified,
        latest_finalized,
        blocks: [(block_root, anchor_block)].into(),
        states: [(block_root, anchor_state)].into(),
        latest_known_votes: HashMap::new(),
        latest_new_votes: HashMap::new(),
        blocks_queue: HashMap::new(),
    }
}

pub fn get_fork_choice_head(
    store: &Store,
    mut root: Root,
    latest_votes: &HashMap<ValidatorIndex, Checkpoint>,
    min_votes: usize,
) -> Root {
    if root.0.is_zero() {
        root = store
            .blocks
            .iter()
            .min_by_key(|(_, block)| block.message.block.slot)
            .map(|(r, _)| *r)
            .expect("Error: Empty block.");
    }
    let mut vote_weights: HashMap<Root, usize> = HashMap::new();
    let root_slot = store.blocks[&root].message.block.slot;

    // stage 1
    for v in latest_votes.values() {
        if let Some(block) = store.blocks.get(&v.root) {
            let mut curr = v.root;

            let mut curr_slot = block.message.block.slot;

            while curr_slot > root_slot {
                *vote_weights.entry(curr).or_insert(0) += 1;

                if let Some(parent_block) = store.blocks.get(&curr) {
                    curr = parent_block.message.block.parent_root;
                    if curr.0.is_zero() {
                        break;
                    }
                    if let Some(next_block) = store.blocks.get(&curr) {
                        curr_slot = next_block.message.block.slot;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
    }

    // stage 2
    let mut child_map: HashMap<Root, Vec<Root>> = HashMap::new();
    for (block_hash, block) in &store.blocks {
        if !block.message.block.parent_root.0.is_zero() {
            if vote_weights.get(block_hash).copied().unwrap_or(0) >= min_votes {
                child_map
                    .entry(block.message.block.parent_root)
                    .or_default()
                    .push(*block_hash);
            }
        }
    }

    // stage 3
    let mut curr = root;
    loop {
        let children = match child_map.get(&curr) {
            Some(list) if !list.is_empty() => list,
            _ => return curr,
        };

        curr = *children
            .iter()
            .max_by(|&&a, &&b| {
                let wa = vote_weights.get(&a).copied().unwrap_or(0);
                let wb = vote_weights.get(&b).copied().unwrap_or(0);
                let slot_a = store.blocks[&a].message.block.slot;
                let slot_b = store.blocks[&b].message.block.slot;
                wa.cmp(&wb)
                    .then_with(|| slot_b.cmp(&slot_a))
                    .then_with(|| a.cmp(&b))
            })
            .unwrap();
    }
}

pub fn get_latest_justified(states: &HashMap<Root, State>) -> Option<&Checkpoint> {
    states
        .values()
        .map(|state| &state.latest_justified)
        .max_by_key(|checkpoint| checkpoint.slot)
}

pub fn update_head(store: &mut Store) {
    if let Some(latest_justified) = get_latest_justified(&store.states) {
        if latest_justified.slot > store.latest_justified.slot {
            store.latest_justified = latest_justified.clone();
        }
    }

    let new_head = get_fork_choice_head(
        store,
        store.latest_justified.root,
        &store.latest_known_votes,
        0,
    );
    store.head = new_head;

    if let Some(state) = store.states.get(&store.head) {
        if state.latest_finalized.slot > store.latest_finalized.slot {
            store.latest_finalized = state.latest_finalized.clone();
        }
    }
}

pub fn update_safe_target(store: &mut Store) {
    let n_validators = if let Some(state) = store.states.get(&store.head) {
        let mut count: u64 = 0;
        let mut i: u64 = 0;
        loop {
            match state.validators.get(i) {
                Ok(_) => {
                    count += 1;
                    i += 1;
                }
                Err(_) => break,
            }
        }
        count as usize
    } else {
        0
    };

    let min_score = (n_validators * 2 + 2) / 3;
    let root = store.latest_justified.root;
    store.safe_target = get_fork_choice_head(store, root, &store.latest_new_votes, min_score);
}

pub fn accept_new_votes(store: &mut Store) {
    store
        .latest_known_votes
        .extend(store.latest_new_votes.drain());
    update_head(store);
}

pub fn tick_interval(store: &mut Store, has_proposal: bool) {
    store.time += 1;
    let curr_interval = store.time % INTERVALS_PER_SLOT;

    match curr_interval {
        0 if has_proposal => accept_new_votes(store),
        2 => update_safe_target(store),
        _ if curr_interval != 1 => accept_new_votes(store),
        _ => {}
    }
}

pub fn get_vote_target(store: &Store) -> Checkpoint {
    let mut target = store.head;
    let safe_slot = store.blocks[&store.safe_target].message.block.slot;

    for _ in 0..3 {
        if store.blocks[&target].message.block.slot > safe_slot {
            target = store.blocks[&target].message.block.parent_root;
        } else {
            break;
        }
    }

    let final_slot = store.latest_finalized.slot;
    while !store.blocks[&target]
        .message
        .block
        .slot
        .is_justifiable_after(final_slot)
    {
        target = store.blocks[&target].message.block.parent_root;
    }

    let block_target = &store.blocks[&target].message.block;
    Checkpoint {
        root: target,
        slot: block_target.slot,
    }
}

#[inline]
pub fn get_proposal_head(store: &mut Store, slot: Slot) -> Root {
    let slot_time = store.config.genesis_time + (slot.0 * SECONDS_PER_SLOT);

    crate::handlers::on_tick(store, slot_time, true);
    accept_new_votes(store);
    store.head
}
