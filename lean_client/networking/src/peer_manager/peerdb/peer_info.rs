use crate::types::ConnectionState;

/// Per-peer connection information stored in PeerDB.
///
/// Following eth2_libp2p's PeerInfo pattern.
#[derive(Debug, Default)]
pub struct PeerInfo {
    pub state: ConnectionState,
    pub direction: ConnectionDirection,
}

/// Direction of a peer connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectionDirection {
    #[default]
    Unknown,
    Incoming,
    Outgoing,
}
