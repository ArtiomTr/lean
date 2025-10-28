pub mod runner;
pub mod state_transition;
pub mod block_processing;
pub mod vote_processing;

use serde::{Deserialize, Serialize};
use containers::{
    block::SignedBlock,
    config::Config as ContainerConfig,
    state::State,
    vote::SignedVote,
};

#[derive(Debug, Serialize, Deserialize)]
pub struct TestCase<T> {
    pub description: String,
    pub pre: T,
    pub post: Option<T>,
    pub blocks: Option<Vec<SignedBlock>>,
    pub votes: Option<Vec<SignedVote>>,
    pub valid: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TestVector<T> {
    pub test_cases: Vec<TestCase<T>>,
    pub config: ContainerConfig,
}
