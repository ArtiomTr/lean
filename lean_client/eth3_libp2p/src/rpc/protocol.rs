use super::methods::*;
use crate::rpc::codec::SSZSnappyInboundCodec;
use futures::future::BoxFuture;
use futures::prelude::{AsyncRead, AsyncWrite};
use futures::{FutureExt, StreamExt};
use libp2p::core::{InboundUpgrade, UpgradeInfo};
use ssz::{H256, ReadError, SszSize as _, SszWrite as _, WriteError};
use std::io;
use std::sync::Arc;
use std::time::Duration;
use std_ext::ArcExt as _;
use strum::{AsRefStr, Display, EnumString, IntoStaticStr};
use tokio_util::{
    codec::Framed,
    compat::{Compat, FuturesAsyncReadCompatExt},
};
use types::config::Config as ChainConfig;

/// Status message SSZ size: two Checkpoints (epoch u64 + root H256 = 40 bytes each).
pub const STATUS_MESSAGE_SIZE: usize = 80;
pub const ERROR_TYPE_MIN: usize = 0;
pub const ERROR_TYPE_MAX: usize = 256;

pub const SIGNED_BEACON_BLOCK_PHASE0_MIN: usize = 100;
pub const SIGNED_BEACON_BLOCK_MAX: usize = 157_756_000;

const METADATA_V1_SIZE: usize = 16; // seq_number (8) + attnets bitvector[64] (8)
const METADATA_V2_SIZE: usize = 24; // + syncnets bitvector[64] (8)

/// The protocol prefix the RPC protocol id.
const PROTOCOL_PREFIX: &str = "/leanconsensus/req";
/// The number of seconds to wait for the first bytes of a request once a protocol has been
/// established before the stream is terminated.
const REQUEST_TIMEOUT: u64 = 15;

/// Protocol names to be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, AsRefStr, Display)]
#[strum(serialize_all = "snake_case")]
pub enum Protocol {
    /// The Status protocol name.
    Status,
    /// The Goodbye protocol name.
    Goodbye,
    /// The `BlocksByRoot` protocol name.
    #[strum(serialize = "beacon_blocks_by_root")]
    BlocksByRoot,
    /// The `Ping` protocol name.
    Ping,
    /// The `MetaData` protocol name.
    #[strum(serialize = "metadata")]
    MetaData,
}

impl Protocol {
    pub(crate) fn terminator(self) -> Option<ResponseTermination> {
        match self {
            Protocol::Status => None,
            Protocol::Goodbye => None,
            Protocol::BlocksByRoot => Some(ResponseTermination::BlocksByRoot),
            Protocol::Ping => None,
            Protocol::MetaData => None,
        }
    }
}

/// Protocol encodings supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, AsRefStr)]
#[strum(serialize_all = "snake_case")]
pub enum Encoding {
    SSZSnappy,
}

/// All valid protocol name and version combinations.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SupportedProtocol {
    StatusV1,
    StatusV2,
    GoodbyeV1,
    BlocksByRootV1,
    BlocksByRootV2,
    PingV1,
    MetaDataV1,
    MetaDataV2,
}

impl SupportedProtocol {
    pub fn version_string(&self) -> &'static str {
        match self {
            SupportedProtocol::StatusV1 => "1",
            SupportedProtocol::StatusV2 => "2",
            SupportedProtocol::GoodbyeV1 => "1",
            SupportedProtocol::BlocksByRootV1 => "1",
            SupportedProtocol::BlocksByRootV2 => "2",
            SupportedProtocol::PingV1 => "1",
            SupportedProtocol::MetaDataV1 => "1",
            SupportedProtocol::MetaDataV2 => "2",
        }
    }

    pub fn protocol(&self) -> Protocol {
        match self {
            SupportedProtocol::StatusV1 => Protocol::Status,
            SupportedProtocol::StatusV2 => Protocol::Status,
            SupportedProtocol::GoodbyeV1 => Protocol::Goodbye,
            SupportedProtocol::BlocksByRootV1 => Protocol::BlocksByRoot,
            SupportedProtocol::BlocksByRootV2 => Protocol::BlocksByRoot,
            SupportedProtocol::PingV1 => Protocol::Ping,
            SupportedProtocol::MetaDataV1 => Protocol::MetaData,
            SupportedProtocol::MetaDataV2 => Protocol::MetaData,
        }
    }

    fn currently_supported() -> Vec<ProtocolId> {
        vec![
            ProtocolId::new(Self::StatusV2, Encoding::SSZSnappy),
            ProtocolId::new(Self::StatusV1, Encoding::SSZSnappy),
            ProtocolId::new(Self::GoodbyeV1, Encoding::SSZSnappy),
            ProtocolId::new(Self::BlocksByRootV2, Encoding::SSZSnappy),
            ProtocolId::new(Self::BlocksByRootV1, Encoding::SSZSnappy),
            ProtocolId::new(Self::PingV1, Encoding::SSZSnappy),
            ProtocolId::new(Self::MetaDataV2, Encoding::SSZSnappy),
            ProtocolId::new(Self::MetaDataV1, Encoding::SSZSnappy),
        ]
    }
}

impl std::fmt::Display for Encoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let repr = match self {
            Encoding::SSZSnappy => "ssz_snappy",
        };
        f.write_str(repr)
    }
}

#[derive(Debug, Clone)]
pub struct RPCProtocol {
    pub chain_config: Arc<ChainConfig>,
    pub max_rpc_size: usize,
}

impl UpgradeInfo for RPCProtocol {
    type Info = ProtocolId;
    type InfoIter = Vec<Self::Info>;

    fn protocol_info(&self) -> Self::InfoIter {
        SupportedProtocol::currently_supported()
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
    pub fn rpc_request_limits(&self, chain_config: &ChainConfig) -> RpcLimits {
        match self.versioned_protocol.protocol() {
            Protocol::Status => RpcLimits::new(STATUS_MESSAGE_SIZE, STATUS_MESSAGE_SIZE),
            Protocol::Goodbye => {
                RpcLimits::new(GoodbyeReason::SIZE.get(), GoodbyeReason::SIZE.get())
            }
            Protocol::BlocksByRoot => RpcLimits::new(
                0,
                chain_config.max_request_blocks as usize * H256::SIZE.get(),
            ),
            Protocol::Ping => RpcLimits::new(Ping::SIZE.get(), Ping::SIZE.get()),
            Protocol::MetaData => RpcLimits::new(0, 0), // Metadata requests are empty
        }
    }

    /// Returns min and max size for messages of given protocol id responses.
    pub fn rpc_response_limits(&self) -> RpcLimits {
        match self.versioned_protocol.protocol() {
            Protocol::Status => RpcLimits::new(STATUS_MESSAGE_SIZE, STATUS_MESSAGE_SIZE),
            Protocol::Goodbye => RpcLimits::new(0, 0), // Goodbye request has no response
            Protocol::BlocksByRoot => {
                RpcLimits::new(SIGNED_BEACON_BLOCK_PHASE0_MIN, SIGNED_BEACON_BLOCK_MAX)
            }
            Protocol::Ping => RpcLimits::new(Ping::SIZE.get(), Ping::SIZE.get()),
            Protocol::MetaData => RpcLimits::new(METADATA_V1_SIZE, METADATA_V2_SIZE),
        }
    }

    /// Returns `true` if the given `ProtocolId` should expect `context_bytes` in the
    /// beginning of the stream, else returns `false`.
    pub fn has_context_bytes(&self) -> bool {
        match self.versioned_protocol {
            SupportedProtocol::BlocksByRootV2 => true,
            SupportedProtocol::StatusV1
            | SupportedProtocol::StatusV2
            | SupportedProtocol::BlocksByRootV1
            | SupportedProtocol::PingV1
            | SupportedProtocol::MetaDataV1
            | SupportedProtocol::MetaDataV2
            | SupportedProtocol::GoodbyeV1 => false,
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

pub type InboundOutput<TSocket> = (RequestType, InboundFramed<TSocket>);
pub type InboundFramed<TSocket> =
    Framed<std::pin::Pin<Box<Compat<TSocket>>>, SSZSnappyInboundCodec>;

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
            let socket = socket.compat();
            let codec = match protocol.encoding {
                Encoding::SSZSnappy => SSZSnappyInboundCodec::new(
                    self.chain_config.clone_arc(),
                    protocol,
                    self.max_rpc_size,
                ),
            };

            let socket = Framed::new(Box::pin(socket), codec);

            match versioned_protocol {
                SupportedProtocol::MetaDataV1 => {
                    Ok((RequestType::MetaData(MetadataRequest::new_v1()), socket))
                }
                SupportedProtocol::MetaDataV2 => {
                    Ok((RequestType::MetaData(MetadataRequest::new_v2()), socket))
                }
                _ => {
                    match tokio::time::timeout(
                        Duration::from_secs(REQUEST_TIMEOUT),
                        socket.into_future(),
                    )
                    .await
                    {
                        Err(e) => Err(RPCError::from(e)),
                        Ok((Some(Ok(request)), stream)) => Ok((request, stream)),
                        Ok((Some(Err(e)), _)) => Err(e),
                        Ok((None, _)) => Err(RPCError::IncompleteStream),
                    }
                }
            }
        }
        .boxed()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RequestType {
    Status(StatusMessage),
    Goodbye(GoodbyeReason),
    BlocksByRoot(BlocksByRootRequest),
    Ping(Ping),
    MetaData(MetadataRequest),
}

impl RequestType {
    /* These functions are used in the handler for stream management */

    /// Gives the corresponding `SupportedProtocol` to this request.
    pub fn versioned_protocol(&self) -> SupportedProtocol {
        match self {
            RequestType::Status(_) => SupportedProtocol::StatusV2,
            RequestType::Goodbye(_) => SupportedProtocol::GoodbyeV1,
            RequestType::BlocksByRoot(_) => SupportedProtocol::BlocksByRootV2,
            RequestType::Ping(_) => SupportedProtocol::PingV1,
            RequestType::MetaData(req) => match req {
                MetadataRequest::V1 => SupportedProtocol::MetaDataV1,
                MetadataRequest::V2 => SupportedProtocol::MetaDataV2,
            },
        }
    }

    pub fn protocol(&self) -> Protocol {
        self.versioned_protocol().protocol()
    }

    /// Maximum number of responses expected for this request.
    pub fn max_responses(&self) -> u64 {
        match self {
            RequestType::Status(_) => 1,
            RequestType::Goodbye(_) => 0,
            RequestType::BlocksByRoot(req) => req.len() as u64,
            RequestType::Ping(_) => 1,
            RequestType::MetaData(_) => 1,
        }
    }

    /// Returns the `ResponseTermination` type associated with the request if a stream gets
    /// terminated.
    pub fn stream_termination(&self) -> ResponseTermination {
        match self {
            RequestType::BlocksByRoot(_) => ResponseTermination::BlocksByRoot,
            RequestType::Status(_) => unreachable!(),
            RequestType::Goodbye(_) => unreachable!(),
            RequestType::Ping(_) => unreachable!(),
            RequestType::MetaData(_) => unreachable!(),
        }
    }

    pub fn supported_protocols(&self) -> Vec<ProtocolId> {
        match self {
            RequestType::Status(_) => vec![
                ProtocolId::new(SupportedProtocol::StatusV2, Encoding::SSZSnappy),
                ProtocolId::new(SupportedProtocol::StatusV1, Encoding::SSZSnappy),
            ],
            RequestType::Goodbye(_) => vec![ProtocolId::new(
                SupportedProtocol::GoodbyeV1,
                Encoding::SSZSnappy,
            )],
            RequestType::BlocksByRoot(_) => vec![
                ProtocolId::new(SupportedProtocol::BlocksByRootV2, Encoding::SSZSnappy),
                ProtocolId::new(SupportedProtocol::BlocksByRootV1, Encoding::SSZSnappy),
            ],
            RequestType::Ping(_) => vec![ProtocolId::new(
                SupportedProtocol::PingV1,
                Encoding::SSZSnappy,
            )],
            RequestType::MetaData(_) => vec![
                ProtocolId::new(SupportedProtocol::MetaDataV2, Encoding::SSZSnappy),
                ProtocolId::new(SupportedProtocol::MetaDataV1, Encoding::SSZSnappy),
            ],
        }
    }

    pub fn expect_exactly_one_response(&self) -> bool {
        match self {
            RequestType::Status(_) => true,
            RequestType::Goodbye(_) => false,
            RequestType::BlocksByRoot(_) => false,
            RequestType::Ping(_) => true,
            RequestType::MetaData(_) => true,
        }
    }
}

/// Error in RPC Encoding/Decoding.
#[derive(Debug, Clone, PartialEq, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum RPCError {
    /// Error when decoding the raw buffer from ssz.
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

impl std::fmt::Display for RequestType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestType::Status(status) => write!(f, "Status Message: {}", status),
            RequestType::Goodbye(reason) => write!(f, "Goodbye: {}", reason),
            RequestType::BlocksByRoot(req) => write!(f, "Blocks by root: {:?}", req),
            RequestType::Ping(ping) => write!(f, "Ping: {}", ping.data),
            RequestType::MetaData(_) => write!(f, "MetaData request"),
        }
    }
}

impl RPCError {
    /// Get a `str` representation of the error.
    pub fn as_static_str(&self) -> &'static str {
        match self {
            RPCError::ErrorResponse(code, ..) => code.into(),
            e => e.into(),
        }
    }
}
