use crate::store::*;
use containers::{
    block::{hash_tree_root, SignedBlock},
    vote::SignedVote,
    ValidatorIndex,
};

#[inline]
pub fn on_tick(store: &mut Store, time: u64, _has_proposal: bool) {
    let elapsed_intervals =
        time.saturating_sub(store.config.genesis_time) * INTERVALS_PER_SLOT / SECONDS_PER_SLOT;
    if store.time < elapsed_intervals {
        store.time = elapsed_intervals;
    }
}

#[inline]
pub fn on_attestation(store: &mut Store, attestation: SignedVote, is_from_block: bool) {
    let key_vald = ValidatorIndex(attestation.data.validator_id.0);
    let vote = attestation.data.target;

    let curr_slot = store.time / INTERVALS_PER_SLOT;
    if vote.slot.0 > curr_slot {
        return;
    }

    if is_from_block {
        if store
            .latest_known_votes
            .get(&key_vald)
            .map_or(true, |v| v.slot < vote.slot)
        {
            store.latest_known_votes.insert(key_vald, vote);
        }
    } else {
        if store
            .latest_new_votes
            .get(&key_vald)
            .map_or(true, |v| v.slot < vote.slot)
        {
            store.latest_new_votes.insert(key_vald, vote);
        }
    }
}

//update
pub fn on_block(store: &mut Store, signed_block: SignedBlock) {
    let block_root = hash_tree_root(&signed_block.message);
    if store.blocks.contains_key(&block_root) {
        return;
    }
    let root = signed_block.message.parent_root;

    let attest = &signed_block.message.body.attestations;
    for i in 0.. {
        match attest.get(i) {
            Ok(attest) => {
                on_attestation(store, attest.clone(), true);
            }
            Err(_) => break,
        }
    }

    // naujas
    let state = match store.states.get(&root) {
        Some(state) => state,
        None => {
            panic!("Err: (Fork-choice::Handlers::OnBlock) No parent state present.");
        }
    };

    let new_state = state.state_transition(signed_block.clone(), true);
    store.blocks.insert(block_root, signed_block);
    store.states.insert(block_root, new_state);
    update_head(store);
}
