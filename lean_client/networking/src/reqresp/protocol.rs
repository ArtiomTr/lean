use std::cmp;

use ssz::SszSize;
use strum::{AsRefStr, Display, EnumString};

use crate::reqresp::message::StatusMessageV1;

/// The protocol prefix for req/resp.
const PROTOCOL_PREFIX: &str = "leanconsensus";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, AsRefStr, Display)]
#[strum(serialize_all = "snake_case")]
pub enum Protocol {
    Status,
    BlocksByRoot,
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
}

/// Supported request/response encodings
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, AsRefStr, Display)]
#[strum(serialize_all = "snake_case")]
pub enum Encoding {
    SSZSnappy,
}

#[derive(Clone, Debug)]
pub struct ProtocolId {
    /// The protocol name and version
    pub versioned_protocol: SupportedProtocol,

    /// The encoding of requests/responses.
    pub encoding: Encoding,

    /// The protocol id that is formed form the above fields.
    protocol_id: String,
}

impl AsRef<str> for ProtocolId {
    fn as_ref(&self) -> &str {
        &self.protocol_id
    }
}

/// Represents the ssz length bounds for request/response messages.
#[derive(Debug, PartialEq)]
pub struct ReqRespLimits {
    pub min: usize,
    pub max: usize,
}

impl ReqRespLimits {
    pub fn new(min: usize, max: usize) -> Self {
        Self { min, max }
    }

    pub fn fixed(size: usize) -> Self {
        Self {
            min: size,
            max: size,
        }
    }

    /// Returns true if the given length is greater than `max_size` or out of
    /// bounds for the given ssz type, returns false otherwise.
    pub fn is_out_of_bounds(&self, length: usize, max_size: usize) -> bool {
        length > cmp::min(self.max, max_size) || length < self.min
    }
}

impl ProtocolId {
    pub fn new(versioned_protocol: SupportedProtocol, encoding: Encoding) -> Self {
        let protocol_id = format!(
            "/{}/{}/{}/{}",
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

    pub fn request_limits(&self) -> ReqRespLimits {
        match self.versioned_protocol.protocol() {
            Protocol::Status => ReqRespLimits::fixed(StatusMessageV1::SIZE.get()),
            Protocol::BlocksByRoot => todo!(),
        }
    }

    pub fn response_limits(&self) -> ReqRespLimits {
        match self.versioned_protocol.protocol() {
            Protocol::Status => ReqRespLimits::fixed(StatusMessageV1::SIZE.get()),
            Protocol::BlocksByRoot => todo!(),
        }
    }
}
