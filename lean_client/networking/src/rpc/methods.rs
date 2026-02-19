use containers::{SignedBlockWithAttestation, Status};
use ssz::H256;

/// Maximum number of block roots per BlocksByRoot request.
pub const MAX_REQUEST_BLOCKS: usize = 1024;

/// Outbound request message types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeanRequest {
    Status(Status),
    BlocksByRoot(Vec<H256>),
}

/// Inbound response message types.
#[derive(Debug, Clone)]
pub enum LeanResponse {
    Status(Status),
    BlocksByRoot(Vec<SignedBlockWithAttestation>),
    Empty,
}
