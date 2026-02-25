//! Available RPC methods types and ids.
use std::fmt::Display;

use crate::types::{EnrAttestationBitfield, EnrSyncCommitteeBitfield};
use containers::{SignedBlockWithAttestation, Slot, Status};
use regex::bytes::Regex;
use serde::Serialize;
use ssz::{
    ContiguousList, DynamicList, H256, ReadError, Size, Ssz, SszRead, SszSize, SszWrite, WriteError,
};
use std::{ops::Deref, sync::Arc};
use strum::IntoStaticStr;
use try_from_iterator::TryFromIterator as _;
use typenum::{U256, Unsigned as _};

/// Maximum length of error message.
pub type MaxErrorLen = U256;

/// Wrapper over SSZ List to represent error message in rpc responses.
#[derive(Debug, Clone)]
pub struct ErrorType(pub ContiguousList<u8, MaxErrorLen>);

impl From<String> for ErrorType {
    fn from(string: String) -> Self {
        Self(ContiguousList::try_from_iter(string.bytes().take(MaxErrorLen::USIZE)).unwrap())
    }
}

impl From<&str> for ErrorType {
    fn from(string: &str) -> Self {
        Self(ContiguousList::try_from_iter(string.bytes().take(MaxErrorLen::USIZE)).unwrap())
    }
}

impl Deref for ErrorType {
    type Target = ContiguousList<u8, MaxErrorLen>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Display for ErrorType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        #[allow(clippy::invalid_regex)]
        let re = Regex::new("\\p{C}").expect("Regex is valid");
        let error_type_str =
            String::from_utf8_lossy(&re.replace_all(self.0.deref(), &b""[..])).to_string();
        write!(f, "{}", error_type_str)
    }
}

/* Request/Response data structures for RPC methods */

/* Requests */

/// The STATUS request/response handshake message.
/// Uses the Status type from the containers crate.
pub type StatusMessage = Status;

/// The PING request/response message.
#[derive(Copy, Clone, Debug, PartialEq, Ssz)]
#[ssz(derive_hash = false, transparent)]
pub struct Ping {
    /// The metadata sequence number.
    pub data: u64,
}

/// The METADATA request structure.
#[derive(Clone, Debug, PartialEq)]
pub enum MetadataRequest {
    V1,
    V2,
}

impl MetadataRequest {
    pub fn new_v1() -> Self {
        Self::V1
    }

    pub fn new_v2() -> Self {
        Self::V2
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Debug)]
pub enum MetaData {
    V1(MetaDataV1),
    V2(MetaDataV2),
}

impl MetaData {
    pub fn seq_number(self) -> u64 {
        match self {
            Self::V1(meta_data) => meta_data.seq_number,
            Self::V2(meta_data) => meta_data.seq_number,
        }
    }

    pub fn attnets(self) -> EnrAttestationBitfield {
        match self {
            Self::V1(meta_data) => meta_data.attnets,
            Self::V2(meta_data) => meta_data.attnets,
        }
    }

    pub fn syncnets(self) -> Option<EnrSyncCommitteeBitfield> {
        match self {
            Self::V1(_) => None,
            Self::V2(meta_data) => Some(meta_data.syncnets),
        }
    }

    pub fn seq_number_mut(&mut self) -> &mut u64 {
        match self {
            Self::V1(meta_data) => &mut meta_data.seq_number,
            Self::V2(meta_data) => &mut meta_data.seq_number,
        }
    }

    pub fn attnets_mut(&mut self) -> &mut EnrAttestationBitfield {
        match self {
            Self::V1(meta_data) => &mut meta_data.attnets,
            Self::V2(meta_data) => &mut meta_data.attnets,
        }
    }

    pub fn syncnets_mut(&mut self) -> Option<&mut EnrSyncCommitteeBitfield> {
        match self {
            Self::V1(_) => None,
            Self::V2(meta_data) => Some(&mut meta_data.syncnets),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Ssz)]
#[serde(deny_unknown_fields)]
#[ssz(derive_hash = false)]
pub struct MetaDataV1 {
    /// A sequential counter indicating when data gets modified.
    pub seq_number: u64,
    /// The persistent attestation subnet bitfield.
    pub attnets: EnrAttestationBitfield,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Ssz)]
#[serde(deny_unknown_fields)]
#[ssz(derive_hash = false)]
pub struct MetaDataV2 {
    /// A sequential counter indicating when data gets modified.
    pub seq_number: u64,
    /// The persistent attestation subnet bitfield.
    pub attnets: EnrAttestationBitfield,
    /// The persistent sync committee bitfield.
    pub syncnets: EnrSyncCommitteeBitfield,
}

impl MetaData {
    /// Returns a V1 MetaData response from self.
    pub fn metadata_v1(&self) -> Self {
        match self {
            md @ MetaData::V1(_) => md.clone(),
            MetaData::V2(metadata) => MetaData::V1(MetaDataV1 {
                seq_number: metadata.seq_number,
                attnets: metadata.attnets.clone(),
            }),
        }
    }

    /// Returns a V2 MetaData response from self by filling unavailable fields with default.
    pub fn metadata_v2(&self) -> Self {
        match self {
            MetaData::V1(metadata) => MetaData::V2(MetaDataV2 {
                seq_number: metadata.seq_number,
                attnets: metadata.attnets.clone(),
                syncnets: Default::default(),
            }),
            md @ MetaData::V2(_) => md.clone(),
        }
    }

    pub fn to_ssz(&self) -> Result<Vec<u8>, WriteError> {
        match self {
            MetaData::V1(md) => md.to_ssz(),
            MetaData::V2(md) => md.to_ssz(),
        }
    }
}

/// The reason given for a `Goodbye` message.
///
/// Note: any unknown `u64::into(n)` will resolve to `Goodbye::Unknown` for any unknown `n`,
/// however `GoodbyeReason::Unknown.into()` will go into `0_u64`. Therefore de-serializing then
/// re-serializing may not return the same bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoodbyeReason {
    /// This node has shutdown.
    ClientShutdown = 1,

    /// Incompatible networks.
    IrrelevantNetwork = 2,

    /// Error/fault in the RPC.
    Fault = 3,

    /// Teku uses this code for not being able to verify a network.
    UnableToVerifyNetwork = 128,

    /// The node has too many connected peers.
    TooManyPeers = 129,

    /// Scored poorly.
    BadScore = 250,

    /// The peer is banned
    Banned = 251,

    /// The IP address the peer is using is banned.
    BannedIP = 252,

    /// Unknown reason.
    Unknown = 0,
}

impl SszSize for GoodbyeReason {
    const SIZE: Size = u64::SIZE;
}

impl<C> SszRead<C> for GoodbyeReason {
    #[inline]
    fn from_ssz_unchecked(context: &C, bytes: &[u8]) -> Result<Self, ReadError> {
        u64::from_ssz_unchecked(context, bytes).map(Into::into)
    }
}

impl SszWrite for GoodbyeReason {
    #[inline]
    fn write_fixed(&self, bytes: &mut [u8]) {
        (*self as u64).write_fixed(bytes);
    }
}

impl From<u64> for GoodbyeReason {
    fn from(id: u64) -> GoodbyeReason {
        match id {
            1 => GoodbyeReason::ClientShutdown,
            2 => GoodbyeReason::IrrelevantNetwork,
            3 => GoodbyeReason::Fault,
            128 => GoodbyeReason::UnableToVerifyNetwork,
            129 => GoodbyeReason::TooManyPeers,
            250 => GoodbyeReason::BadScore,
            251 => GoodbyeReason::Banned,
            252 => GoodbyeReason::BannedIP,
            _ => GoodbyeReason::Unknown,
        }
    }
}

impl From<GoodbyeReason> for u64 {
    fn from(reason: GoodbyeReason) -> u64 {
        reason as u64
    }
}

/// Request a number of beacon block bodies from a peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlocksByRootRequest {
    /// The list of beacon block roots being requested.
    pub block_roots: DynamicList<H256>,
}

impl BlocksByRootRequest {
    pub fn new(block_roots: impl Iterator<Item = H256>, max_blocks: usize) -> Self {
        let block_roots = DynamicList::from_iter_with_maximum(block_roots, max_blocks);
        Self { block_roots }
    }

    pub fn len(&self) -> usize {
        self.block_roots.len()
    }

    pub fn block_roots(self) -> DynamicList<H256> {
        self.block_roots
    }
}

/* RPC Handling and Grouping */
// Collection of enums and structs used by the Codecs to encode/decode RPC messages

#[derive(Debug, Clone, PartialEq)]
pub enum RpcSuccessResponse {
    /// A HELLO message.
    Status(StatusMessage),

    /// A response to a get BLOCKS_BY_ROOT request.
    BlocksByRoot(Arc<SignedBlockWithAttestation>),

    /// A PONG response to a PING request.
    Pong(Ping),

    /// A response to a META_DATA request.
    MetaData(MetaData),
}

/// Indicates which response is being terminated by a stream termination response.
#[derive(Debug, Clone)]
pub enum ResponseTermination {
    /// Blocks by root stream termination.
    BlocksByRoot,
}

impl ResponseTermination {
    pub fn as_protocol(&self) -> Protocol {
        match self {
            ResponseTermination::BlocksByRoot => Protocol::BlocksByRoot,
        }
    }
}

/// The structured response containing a result/code indicating success or failure
/// and the contents of the response
#[derive(Debug, Clone)]
pub enum RpcResponse {
    /// The response is a successful.
    Success(RpcSuccessResponse),

    Error(RpcErrorResponse, ErrorType),

    /// Received a stream termination indicating which response is being terminated.
    StreamTermination(ResponseTermination),
}

/// The code assigned to an erroneous `RPCResponse`.
#[derive(Debug, Clone, Copy, PartialEq, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum RpcErrorResponse {
    RateLimited,
    BlobsNotFoundForBlock,
    InvalidRequest,
    ServerError,
    /// Error spec'd to indicate that a peer does not have blocks on a requested range.
    ResourceUnavailable,
    Unknown,
}

impl RpcResponse {
    /// Used to encode the response in the codec.
    pub fn as_u8(&self) -> Option<u8> {
        match self {
            RpcResponse::Success(_) => Some(0),
            RpcResponse::Error(code, _) => Some(code.as_u8()),
            RpcResponse::StreamTermination(_) => None,
        }
    }

    /// Tells the codec whether to decode as an RPCResponse or an error.
    pub fn is_response(response_code: u8) -> bool {
        matches!(response_code, 0)
    }

    /// Builds an RPCCodedResponse from a response code and an ErrorMessage
    pub fn from_error(response_code: u8, err: ErrorType) -> Self {
        let code = match response_code {
            1 => RpcErrorResponse::InvalidRequest,
            2 => RpcErrorResponse::ServerError,
            3 => RpcErrorResponse::ResourceUnavailable,
            139 => RpcErrorResponse::RateLimited,
            140 => RpcErrorResponse::BlobsNotFoundForBlock,
            _ => RpcErrorResponse::Unknown,
        };
        RpcResponse::Error(code, err)
    }

    /// Returns true if this response always terminates the stream.
    pub fn close_after(&self) -> bool {
        !matches!(self, RpcResponse::Success(_))
    }
}

impl RpcErrorResponse {
    fn as_u8(&self) -> u8 {
        match self {
            RpcErrorResponse::InvalidRequest => 1,
            RpcErrorResponse::ServerError => 2,
            RpcErrorResponse::ResourceUnavailable => 3,
            RpcErrorResponse::Unknown => 255,
            RpcErrorResponse::RateLimited => 139,
            RpcErrorResponse::BlobsNotFoundForBlock => 140,
        }
    }
}

use super::Protocol;
impl RpcSuccessResponse {
    pub fn protocol(&self) -> Protocol {
        match self {
            RpcSuccessResponse::Status(_) => Protocol::Status,
            RpcSuccessResponse::BlocksByRoot(_) => Protocol::BlocksByRoot,
            RpcSuccessResponse::Pong(_) => Protocol::Ping,
            RpcSuccessResponse::MetaData(_) => Protocol::MetaData,
        }
    }

    pub fn slot(&self) -> Option<Slot> {
        match self {
            RpcSuccessResponse::BlocksByRoot(block) => Some(block.message.block.slot),
            RpcSuccessResponse::MetaData(_)
            | RpcSuccessResponse::Status(_)
            | RpcSuccessResponse::Pong(_) => None,
        }
    }
}

impl std::fmt::Display for RpcErrorResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let repr = match self {
            RpcErrorResponse::InvalidRequest => "The request was invalid",
            RpcErrorResponse::ResourceUnavailable => "Resource unavailable",
            RpcErrorResponse::ServerError => "Server error occurred",
            RpcErrorResponse::Unknown => "Unknown error occurred",
            RpcErrorResponse::RateLimited => "Rate limited",
            RpcErrorResponse::BlobsNotFoundForBlock => "No blobs for the given root",
        };
        f.write_str(repr)
    }
}

impl std::fmt::Display for StatusMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Status Message: Finalized: {:?}, Head: {:?}",
            self.finalized, self.head
        )
    }
}

impl std::fmt::Display for RpcSuccessResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcSuccessResponse::Status(status) => write!(f, "{}", status),
            RpcSuccessResponse::BlocksByRoot(block) => {
                write!(f, "BlocksByRoot: Block slot: {}", block.message.block.slot)
            }
            RpcSuccessResponse::Pong(ping) => write!(f, "Pong: {}", ping.data),
            RpcSuccessResponse::MetaData(metadata) => {
                write!(f, "Metadata: {}", metadata.seq_number())
            }
        }
    }
}

impl std::fmt::Display for RpcResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcResponse::Success(res) => write!(f, "{}", res),
            RpcResponse::Error(code, err) => write!(f, "{}: {}", code, err),
            RpcResponse::StreamTermination(_) => write!(f, "Stream Termination"),
        }
    }
}

impl std::fmt::Display for GoodbyeReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GoodbyeReason::ClientShutdown => write!(f, "Client Shutdown"),
            GoodbyeReason::IrrelevantNetwork => write!(f, "Irrelevant Network"),
            GoodbyeReason::Fault => write!(f, "Fault"),
            GoodbyeReason::UnableToVerifyNetwork => write!(f, "Unable to verify network"),
            GoodbyeReason::TooManyPeers => write!(f, "Too many peers"),
            GoodbyeReason::BadScore => write!(f, "Bad Score"),
            GoodbyeReason::Banned => write!(f, "Banned"),
            GoodbyeReason::BannedIP => write!(f, "BannedIP"),
            GoodbyeReason::Unknown => write!(f, "Unknown Reason"),
        }
    }
}
