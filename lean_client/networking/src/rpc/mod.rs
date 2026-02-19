pub mod codec;
pub mod methods;
pub mod protocol;

pub use codec::{LeanCodec, RESP_TIMEOUT, TTFB_TIMEOUT};
pub use methods::{LeanRequest, LeanResponse};
pub use protocol::{BLOCKS_BY_ROOT_PROTOCOL_V1, LeanProtocol, STATUS_PROTOCOL_V1};

use libp2p::request_response::{Behaviour as RequestResponse, Config, Event, ProtocolSupport};

/// The lean ethereum request/response behaviour type.
pub type Rpc = RequestResponse<LeanCodec>;

/// Events from the Rpc behaviour.
pub type RpcEvent = Event<LeanRequest, LeanResponse>;

/// Build an `Rpc` instance with the default lean ethereum protocols.
pub fn build_rpc() -> Rpc {
    let protocols = vec![
        (
            LeanProtocol(STATUS_PROTOCOL_V1.to_string()),
            ProtocolSupport::Full,
        ),
        (
            LeanProtocol(BLOCKS_BY_ROOT_PROTOCOL_V1.to_string()),
            ProtocolSupport::Full,
        ),
    ];

    // Configure request timeout per leanSpec
    let config = Config::default().with_request_timeout(RESP_TIMEOUT);

    RequestResponse::with_codec(LeanCodec::default(), protocols, config)
}
