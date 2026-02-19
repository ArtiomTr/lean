use std::collections::HashMap;
use std::sync::Arc;

use libp2p_identity::PeerId;
use parking_lot::RwLock;

use super::ConnectionState;

/// Shared network state, accessible from multiple components.
///
/// Following the eth2_libp2p NetworkGlobals pattern, this provides
/// thread-safe access to peer connection state and network metadata.
#[derive(Debug, Clone)]
pub struct NetworkGlobals {
    pub peers: Arc<RwLock<HashMap<PeerId, ConnectionState>>>,
}

impl NetworkGlobals {
    pub fn new() -> Self {
        NetworkGlobals {
            peers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn connected_peer_count(&self) -> usize {
        self.peers
            .read()
            .values()
            .filter(|s| **s == ConnectionState::Connected)
            .count()
    }

    pub fn update_peer_state(&self, peer_id: PeerId, state: ConnectionState) {
        self.peers.write().insert(peer_id, state);
    }

    pub fn peer_state(&self, peer_id: &PeerId) -> Option<ConnectionState> {
        self.peers.read().get(peer_id).copied()
    }
}

impl Default for NetworkGlobals {
    fn default() -> Self {
        Self::new()
    }
}
