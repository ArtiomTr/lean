use containers::SignedBlockWithAttestation;

use crate::rpc::methods::{ResponseTermination, RpcResponse, RpcSuccessResponse, StatusMessage};
use std::sync::Arc;

pub type Id = usize;

/// Identifier of a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AppRequestId {
    Application(Id),
    Internal,
}

/// The type of RPC responses the Behaviour informs it has received, and allows for sending.
///
// NOTE: This is an application-level wrapper over the lower network level responses that can be
//       sent. The main difference is the absense of Pong and Metadata, which don't leave the
//       Behaviour. For all protocol responses managed by RPC see `RPCResponse` and
//       `RPCCodedResponse`.
#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    /// A Status message.
    Status(StatusMessage),
    /// A response to a get BLOCKS_BY_ROOT request. A None response signals the end of the batch.
    BlocksByRoot(Option<Arc<SignedBlockWithAttestation>>),
}

impl std::convert::From<Response> for RpcResponse {
    fn from(resp: Response) -> RpcResponse {
        match resp {
            Response::BlocksByRoot(r) => match r {
                Some(b) => RpcResponse::Success(RpcSuccessResponse::BlocksByRoot(b)),
                None => RpcResponse::StreamTermination(ResponseTermination::BlocksByRoot),
            },
            Response::Status(s) => RpcResponse::Success(RpcSuccessResponse::Status(s)),
        }
    }
}
