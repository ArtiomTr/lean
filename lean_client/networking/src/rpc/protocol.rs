/// Protocol identifier for lean ethereum req/resp.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LeanProtocol(pub String);

impl AsRef<str> for LeanProtocol {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

pub const STATUS_PROTOCOL_V1: &str = "/leanconsensus/req/status/1/ssz_snappy";
pub const BLOCKS_BY_ROOT_PROTOCOL_V1: &str = "/leanconsensus/req/blocks_by_root/1/ssz_snappy";

/// All supported request types for lean ethereum req/resp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestType {
    Status,
    BlocksByRoot,
}

impl RequestType {
    /// Detect request type from protocol string.
    pub fn from_protocol(protocol: &str) -> Option<Self> {
        if protocol.contains("status") {
            Some(RequestType::Status)
        } else if protocol.contains("blocks_by_root") {
            Some(RequestType::BlocksByRoot)
        } else {
            None
        }
    }
}
