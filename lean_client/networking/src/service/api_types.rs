/// Application-level request identifier.
///
/// Following eth2_libp2p's AppRequestId pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AppRequestId {
    /// Request originating from the application layer.
    Application(u64),
    /// Internal request (e.g. status handshake).
    Internal,
}
