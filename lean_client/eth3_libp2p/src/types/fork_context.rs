use std::sync::Arc;

use helper_functions::misc;
use types::{
    config::Config,
    nonstandard::Phase,
    phase0::primitives::{Epoch, ForkDigest, H256},
};

/// Simplified fork context for the lean client.
///
/// The lean client operates on a single fixed fork (no fork transitions).
/// ForkContext holds the chain config and a single fork digest.
#[derive(Debug)]
pub struct ForkContext {
    chain_config: Arc<Config>,
    fork_digest: ForkDigest,
}

impl ForkContext {
    /// Creates a new `ForkContext` with the given genesis_validators_root.
    pub fn new(config: &Arc<Config>, genesis_validators_root: H256) -> Self {
        let fork_digest = misc::compute_fork_digest(config, genesis_validators_root, 0);
        Self {
            chain_config: config.clone(),
            fork_digest,
        }
    }

    /// Returns a dummy fork context for testing.
    pub fn dummy(config: &Arc<Config>) -> ForkContext {
        Self::new(config, H256::zero())
    }

    /// Returns the current fork name (always Phase0 for lean client).
    pub fn current_fork_name(&self) -> Phase {
        Phase::Phase0
    }

    /// Returns the current fork epoch (always 0 for lean client).
    pub fn current_fork_epoch(&self) -> Epoch {
        0
    }

    /// Return the current fork digest.
    pub fn current_fork_digest(&self) -> ForkDigest {
        self.fork_digest
    }

    /// Returns the next fork digest. None since lean client has no forks.
    pub fn next_fork_digest(&self) -> Option<ForkDigest> {
        None
    }

    /// Returns the next fork. None since lean client has no forks.
    pub fn next_fork(&self) -> Option<(Phase, ForkDigest, Epoch)> {
        None
    }

    /// No-op: lean client has no fork transitions.
    pub fn update_current_fork(&self) {}

    /// Returns the genesis fork digest.
    pub fn genesis_context_bytes(&self) -> ForkDigest {
        self.fork_digest
    }

    /// Returns Phase0 if context matches the single fork digest, None otherwise.
    pub fn get_fork_from_context_bytes(&self, context: ForkDigest) -> Option<Phase> {
        if context == self.fork_digest {
            Some(Phase::Phase0)
        } else {
            None
        }
    }

    /// Returns the single fork digest for any epoch.
    pub fn context_bytes(&self, _epoch: Epoch) -> ForkDigest {
        self.fork_digest
    }

    /// Returns the single fork digest for any phase.
    pub fn to_context_bytes(&self, _phase: Phase) -> ForkDigest {
        self.fork_digest
    }

    /// Returns all fork digests (just one for lean client).
    pub fn all_fork_digests(&self) -> Vec<ForkDigest> {
        vec![self.fork_digest]
    }

    /// Returns all fork epochs (just epoch 0 for lean client).
    pub fn all_fork_epochs(&self) -> Vec<Epoch> {
        vec![0]
    }

    pub fn chain_config(&self) -> &Arc<Config> {
        &self.chain_config
    }
}
