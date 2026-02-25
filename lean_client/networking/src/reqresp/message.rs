use std::sync::Arc;

use containers::{Checkpoint, SignedBlockWithAttestation};
use ssz::Ssz;

#[derive(Clone, Debug)]
pub enum RequestType {
    Status(StatusMessage),
    BlocksByRoot(BlocksByRoot),
}

/// The Status message, used by clients to share their chain state.
///
/// This is the first message sent upon a new connection and is essential for
/// the peer-to-peer handshake. It allows nodes to verify compatibility and
/// determine if they are on the same chain.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StatusMessage {
    V1(StatusMessageV1),
}

impl StatusMessage {
    pub fn finalized(&self) -> Checkpoint {
        match self {
            Self::V1(StatusMessageV1 { finalized, .. }) => *finalized,
        }
    }

    pub fn head(&self) -> Checkpoint {
        match self {
            Self::V1(StatusMessageV1 { head, .. }) => *head,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Ssz)]
#[ssz(derive_hash = false)]
pub struct StatusMessageV1 {
    /// The client's latest finalized checkpoint.
    pub finalized: Checkpoint,

    /// The client's current head checkpoint.
    pub head: Checkpoint,
}

pub enum BlocksByRoot {}

#[derive(Debug, Clone)]
pub enum ResponseType {
    /// The response is successfull.
    Success(SuccessResponse),

    Error(ErrorResponse),
}

#[derive(Debug, Clone, PartialEq)]
pub enum SuccessResponse {
    /// A HELLO message.
    Status(StatusMessage),

    /// A response to a get BLOCKS_BY_ROOT request.
    BlocksByRoot(Arc<SignedBlockWithAttestation>),

    /// Received a stream termination indicating which response is being terminated.
    StreamTermination(ResponseTermination),
}

#[derive(Debug, Clone)]
pub enum ErrorResponse {}

/// Indicates which response is being terminated by a stream termination response.
#[derive(Debug, Clone)]
pub enum ResponseTermination {
    /// Blocks by root stream termination.
    BlocksByRoot,
}
