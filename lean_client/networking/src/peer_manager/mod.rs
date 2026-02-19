pub mod peerdb;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use libp2p_identity::PeerId;

use crate::types::ConnectionState;
use peerdb::PeerDB;

/// Events emitted by the peer manager.
///
/// Following eth2_libp2p's PeerManagerEvent pattern.
#[derive(Debug)]
pub enum PeerManagerEvent {
    PeerConnectedIncoming(PeerId),
    PeerConnectedOutgoing(PeerId),
    PeerDisconnected(PeerId),
}

/// Manages peer connections, state, and reputation.
///
/// Wraps the peer database and provides methods for querying
/// and updating peer connection state.
pub struct PeerManager {
    db: PeerDB,
    peer_count: Arc<AtomicU64>,
}

impl PeerManager {
    pub fn new(peer_count: Arc<AtomicU64>) -> Self {
        PeerManager {
            db: PeerDB::new(),
            peer_count,
        }
    }

    pub fn on_connect_incoming(&mut self, peer_id: PeerId) {
        self.db.update_state(peer_id, ConnectionState::Connected);
        self.update_count();
    }

    pub fn on_connect_outgoing(&mut self, peer_id: PeerId) {
        self.db.update_state(peer_id, ConnectionState::Connected);
        self.update_count();
    }

    pub fn on_disconnect(&mut self, peer_id: PeerId) {
        self.db.update_state(peer_id, ConnectionState::Disconnected);
        self.update_count();
    }

    pub fn on_dialing(&mut self, peer_id: PeerId) {
        self.db.update_state(peer_id, ConnectionState::Connecting);
    }

    pub fn peer_state(&self, peer_id: &PeerId) -> Option<ConnectionState> {
        self.db.peer_state(peer_id)
    }

    pub fn connected_peers(&self) -> Vec<PeerId> {
        self.db.connected_peers()
    }

    pub fn connected_peer_count(&self) -> usize {
        self.db.connected_peer_count()
    }

    pub fn peer_table(&self) -> HashMap<PeerId, ConnectionState> {
        self.db.all_peers()
    }

    fn update_count(&self) {
        self.peer_count
            .store(self.db.connected_peer_count() as u64, Ordering::Relaxed);
    }
}
