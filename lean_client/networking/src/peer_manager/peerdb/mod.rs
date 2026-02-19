pub mod peer_info;

use std::collections::HashMap;

use libp2p_identity::PeerId;

use crate::types::ConnectionState;
use peer_info::PeerInfo;

/// In-memory database of known peers and their connection state.
///
/// Following eth2_libp2p's PeerDB pattern.
pub struct PeerDB {
    peers: HashMap<PeerId, PeerInfo>,
}

impl PeerDB {
    pub fn new() -> Self {
        PeerDB {
            peers: HashMap::new(),
        }
    }

    pub fn update_state(&mut self, peer_id: PeerId, state: ConnectionState) {
        self.peers.entry(peer_id).or_default().state = state;
    }

    pub fn peer_state(&self, peer_id: &PeerId) -> Option<ConnectionState> {
        self.peers.get(peer_id).map(|p| p.state)
    }

    pub fn connected_peers(&self) -> Vec<PeerId> {
        self.peers
            .iter()
            .filter(|(_, p)| p.state == ConnectionState::Connected)
            .map(|(id, _)| *id)
            .collect()
    }

    pub fn connected_peer_count(&self) -> usize {
        self.peers
            .values()
            .filter(|p| p.state == ConnectionState::Connected)
            .count()
    }

    pub fn all_peers(&self) -> HashMap<PeerId, ConnectionState> {
        self.peers.iter().map(|(id, p)| (*id, p.state)).collect()
    }
}

impl Default for PeerDB {
    fn default() -> Self {
        Self::new()
    }
}
