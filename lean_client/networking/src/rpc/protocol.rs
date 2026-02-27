use super::methods::*;
use crate::rpc::codec::SSZSnappyInboundCodec;
use futures::future::BoxFuture;
use futures::prelude::{AsyncRead, AsyncWrite};
use futures::{FutureExt, StreamExt};
// use helper_functions::misc;
use libp2p::core::{InboundUpgrade, UpgradeInfo};
use ssz::{H256, ReadError, SszSize as _, WriteError};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use std::{fmt, io};
use strum::{AsRefStr, Display, EnumString, IntoStaticStr};
use tokio_util::{
    codec::Framed,
    compat::{Compat, FuturesAsyncReadCompatExt},
};

pub const SIGNED_BEACON_BLOCK_PHASE0_MIN: usize = 404;
pub const SIGNED_BEACON_BLOCK_PHASE0_MAX: usize = 157756;
pub const SIGNED_BEACON_BLOCK_ALTAIR_MAX: usize = 157916;
pub const SIGNED_BEACON_BLOCK_BELLATRIX_MAX: usize = 1125899911195388;

pub const BLOB_SIDECAR_MIN: usize = 131928;
pub const BLOB_SIDECAR_MAX: usize = 131928;

pub const BLOB_SIDECAR_MINIMAL_MIN: usize = 131704;
pub const BLOB_SIDECAR_MINIMAL_MAX: usize = 131704;

pub const ERROR_TYPE_MIN: usize = 0;
pub const ERROR_TYPE_MAX: usize = 256;

// pub(crate) const MAX_RPC_SIZE_POST_EIP4844: usize = 10 * 1_048_576; // 10M

/// The protocol prefix the RPC protocol id.
const PROTOCOL_PREFIX: &str = "/leanconsensus";
/// The number of seconds to wait for the first bytes of a request once a protocol has been
/// established before the stream is terminated.
const REQUEST_TIMEOUT: u64 = 15;

/// Protocol names to be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, AsRefStr, Display)]
#[strum(serialize_all = "snake_case")]
pub enum Protocol {
    /// The Status protocol name.
    Status,
    /// The `BlocksByRoot` protocol name.
    BlocksByRoot,
}

impl Protocol {
    pub(crate) fn terminator(self) -> Option<ResponseTermination> {
        match self {
            Protocol::Status => None,
            Protocol::BlocksByRoot => Some(ResponseTermination::BlocksByRoot),
        }
    }
}

/// RPC Encondings supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, AsRefStr)]
#[strum(serialize_all = "snake_case")]
pub enum Encoding {
    SSZSnappy,
}

/// All valid protocol name and version combinations.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SupportedProtocol {
    StatusV1,
    BlocksByRootV1,
}

impl SupportedProtocol {
    pub fn version_string(&self) -> &'static str {
        match self {
            SupportedProtocol::StatusV1 => "1",
            SupportedProtocol::BlocksByRootV1 => "1",
        }
    }

    pub fn protocol(&self) -> Protocol {
        match self {
            SupportedProtocol::StatusV1 => Protocol::Status,
            SupportedProtocol::BlocksByRootV1 => Protocol::BlocksByRoot,
        }
    }

    fn currently_supported() -> Vec<ProtocolId> {
        let mut supported = vec![
            ProtocolId::new(Self::StatusV1, Encoding::SSZSnappy),
            ProtocolId::new(Self::BlocksByRootV1, Encoding::SSZSnappy),
        ];
        supported
    }
}

impl fmt::Display for Encoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let repr = match self {
            Encoding::SSZSnappy => "ssz_snappy",
        };
        f.write_str(repr)
    }
}

#[derive(Debug, Clone)]
pub struct RPCProtocol {
    pub max_rpc_size: usize,
}

impl UpgradeInfo for RPCProtocol {
    type Info = ProtocolId;
    type InfoIter = Vec<Self::Info>;

    /// The list of supported RPC protocols.
    fn protocol_info(&self) -> Self::InfoIter {
        let mut supported_protocols = SupportedProtocol::currently_supported();
        supported_protocols
    }
}

/// Represents the ssz length bounds for RPC messages.
#[derive(Debug, PartialEq)]
pub struct RpcLimits {
    pub min: usize,
    pub max: usize,
}

impl RpcLimits {
    pub fn new(min: usize, max: usize) -> Self {
        Self { min, max }
    }

    pub fn fixed(size: usize) -> Self {
        Self {
            min: size,
            max: size,
        }
    }

    /// Returns true if the given length is greater than `max_rpc_size` or out of
    /// bounds for the given ssz type, returns false otherwise.
    pub fn is_out_of_bounds(&self, length: usize, max_rpc_size: usize) -> bool {
        length > std::cmp::min(self.max, max_rpc_size) || length < self.min
    }
}

/// Tracks the types in a protocol id.
#[derive(Clone, Debug)]
pub struct ProtocolId {
    /// The protocol name and version
    pub versioned_protocol: SupportedProtocol,

    /// The encoding of the RPC.
    pub encoding: Encoding,

    /// The protocol id that is formed from the above fields.
    protocol_id: String,
}

impl AsRef<str> for ProtocolId {
    fn as_ref(&self) -> &str {
        self.protocol_id.as_ref()
    }
}

impl ProtocolId {
    /// Returns min and max size for messages of given protocol id requests.
    pub fn rpc_request_limits(&self) -> RpcLimits {
        match self.versioned_protocol.protocol() {
            Protocol::Status => RpcLimits::fixed(StatusMessageV1::SIZE.get()),
            Protocol::BlocksByRoot => RpcLimits::new(
                0,
                // TODO(networking): 1024 must be moved to constant
                1024 * H256::SIZE.get(),
            ),
        }
    }

    /// Returns min and max size for messages of given protocol id responses.
    pub fn rpc_response_limits(&self) -> RpcLimits {
        match self.versioned_protocol.protocol() {
            Protocol::Status => RpcLimits::fixed(StatusMessageV1::SIZE.get()),
            Protocol::BlocksByRoot => todo!(),
        }
    }
}

/// An RPC protocol ID.
impl ProtocolId {
    pub fn new(versioned_protocol: SupportedProtocol, encoding: Encoding) -> Self {
        let protocol_id = format!(
            "{}/{}/{}/{}",
            PROTOCOL_PREFIX,
            versioned_protocol.protocol(),
            versioned_protocol.version_string(),
            encoding
        );

        ProtocolId {
            versioned_protocol,
            encoding,
            protocol_id,
        }
    }
}

// The inbound protocol reads the request, decodes it and returns the stream to the protocol
// handler to respond to once ready.

pub type InboundOutput<TSocket> = (RequestType, InboundFramed<TSocket>);
pub type InboundFramed<TSocket> = Framed<Pin<Box<Compat<TSocket>>>, SSZSnappyInboundCodec>;

impl<TSocket> InboundUpgrade<TSocket> for RPCProtocol
where
    TSocket: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Output = InboundOutput<TSocket>;
    type Error = RPCError;
    type Future = BoxFuture<'static, Result<Self::Output, Self::Error>>;

    fn upgrade_inbound(self, socket: TSocket, protocol: ProtocolId) -> Self::Future {
        async move {
            let versioned_protocol = protocol.versioned_protocol;
            // convert the socket to tokio compatible socket
            let socket = socket.compat();
            let codec = match protocol.encoding {
                Encoding::SSZSnappy => SSZSnappyInboundCodec::new(protocol, self.max_rpc_size),
            };

            let socket = Framed::new(Box::pin(socket), codec);

            match tokio::time::timeout(Duration::from_secs(REQUEST_TIMEOUT), socket.into_future())
                .await
            {
                Err(e) => Err(RPCError::from(e)),
                Ok((Some(Ok(request)), stream)) => Ok((request, stream)),
                Ok((Some(Err(e)), _)) => Err(e),
                Ok((None, _)) => Err(RPCError::IncompleteStream),
            }
        }
        .boxed()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RequestType {
    Status(StatusMessage),
    BlocksByRoot(BlocksByRootRequest),
}

/// Implements the encoding per supported protocol for `RPCRequest`.
impl RequestType {
    /* These functions are used in the handler for stream management */

    /// Maximum number of responses expected for this request.
    pub fn max_responses(&self) -> u64 {
        match self {
            RequestType::Status(_) => 1,
            RequestType::BlocksByRoot(req) => req.len() as u64,
        }
    }

    /// Gives the corresponding `SupportedProtocol` to this request.
    pub fn versioned_protocol(&self) -> SupportedProtocol {
        match self {
            RequestType::Status(req) => match req {
                StatusMessage::V1(_) => SupportedProtocol::StatusV1,
            },
            RequestType::BlocksByRoot(req) => match req {
                BlocksByRootRequest::V1(_) => SupportedProtocol::BlocksByRootV1,
            },
        }
    }

    /// Returns the `ResponseTermination` type associated with the request if a stream gets
    /// terminated.
    pub fn stream_termination(&self) -> ResponseTermination {
        match self {
            // this only gets called after `multiple_responses()` returns true. Therefore, only
            // variants that have `multiple_responses()` can have values.
            RequestType::BlocksByRoot(_) => ResponseTermination::BlocksByRoot,
            RequestType::Status(_) => unreachable!(),
        }
    }

    pub fn supported_protocols(&self) -> Vec<ProtocolId> {
        match self {
            // add more protocols when versions/encodings are supported
            RequestType::Status(_) => vec![ProtocolId::new(
                SupportedProtocol::StatusV1,
                Encoding::SSZSnappy,
            )],
            RequestType::BlocksByRoot(_) => vec![ProtocolId::new(
                SupportedProtocol::BlocksByRootV1,
                Encoding::SSZSnappy,
            )],
        }
    }

    pub fn expect_exactly_one_response(&self) -> bool {
        match self {
            RequestType::Status(_) => true,
            RequestType::BlocksByRoot(_) => false,
        }
    }
}

/// Error in RPC Encoding/Decoding.
#[derive(Debug, Clone, PartialEq, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum RPCError {
    /// Error when decoding the raw buffer from ssz.
    // NOTE: in the future a ssz::ReadError should map to an InvalidData error
    #[strum(serialize = "decode_error")]
    SszReadError(ReadError),
    /// Error when encoding data as SSZ.
    #[strum(serialize = "encode_error")]
    SszWriteError(WriteError),
    /// IO Error.
    IoError(String),
    /// The peer returned a valid response but the response indicated an error.
    ErrorResponse(RpcErrorResponse, String),
    /// Timed out waiting for a response.
    StreamTimeout,
    /// Peer does not support the protocol.
    UnsupportedProtocol,
    /// Stream ended unexpectedly.
    IncompleteStream,
    /// Peer sent invalid data.
    InvalidData(String),
    /// An error occurred due to internal reasons. Ex: timer failure.
    InternalError(&'static str),
    /// Negotiation with this peer timed out.
    NegotiationTimeout,
    /// Handler rejected this request.
    HandlerRejected,
    /// We have intentionally disconnected.
    Disconnected,
}

impl From<ReadError> for RPCError {
    #[inline]
    fn from(err: ReadError) -> Self {
        RPCError::SszReadError(err)
    }
}

impl From<WriteError> for RPCError {
    #[inline]
    fn from(err: WriteError) -> Self {
        RPCError::SszWriteError(err)
    }
}

impl From<tokio::time::error::Elapsed> for RPCError {
    fn from(_: tokio::time::error::Elapsed) -> Self {
        RPCError::StreamTimeout
    }
}

impl From<io::Error> for RPCError {
    fn from(err: io::Error) -> Self {
        RPCError::IoError(err.to_string())
    }
}

// Error trait is required for `ProtocolsHandler`
impl std::fmt::Display for RPCError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            RPCError::SszReadError(ref err) => write!(f, "Error while decoding ssz: {:?}", err),
            RPCError::SszWriteError(ref err) => write!(f, "Error while encoding ssz: {:?}", err),
            RPCError::InvalidData(ref err) => write!(f, "Peer sent unexpected data: {}", err),
            RPCError::IoError(ref err) => write!(f, "IO Error: {}", err),
            RPCError::ErrorResponse(ref code, ref reason) => write!(
                f,
                "RPC response was an error: {} with reason: {}",
                code, reason
            ),
            RPCError::StreamTimeout => write!(f, "Stream Timeout"),
            RPCError::UnsupportedProtocol => write!(f, "Peer does not support the protocol"),
            RPCError::IncompleteStream => write!(f, "Stream ended unexpectedly"),
            RPCError::InternalError(ref err) => write!(f, "Internal error: {}", err),
            RPCError::NegotiationTimeout => write!(f, "Negotiation timeout"),
            RPCError::HandlerRejected => write!(f, "Handler rejected the request"),
            RPCError::Disconnected => write!(f, "Gracefully Disconnected"),
        }
    }
}

impl std::error::Error for RPCError {}

impl fmt::Display for RequestType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RequestType::Status(status) => write!(f, "Status Message: {}", status),
            RequestType::BlocksByRoot(req) => write!(f, "Blocks by root: {:?}", req),
        }
    }
}

impl RPCError {
    /// Get a `str` representation of the error.
    /// Used for metrics.
    pub fn as_static_str(&self) -> &'static str {
        match self {
            RPCError::ErrorResponse(code, ..) => code.into(),
            e => e.into(),
        }
    }
}
