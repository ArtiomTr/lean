use std::{collections::HashMap, sync::Arc, time::Duration};

use containers::{SignedBlockWithAttestation, Slot};
use networking::{AppRequestId, InboundRequestId, PeerId, Response, StatusMessage};
use ssz::H256;
use tokio::time::Instant;
use tracing::warn;

use crate::{
    chain::ChainMessage,
    environment::{Effect, Event, Service, ServiceInput, ServiceOutput},
    network::{NetworkEffect, NetworkEvent},
};

#[derive(Debug, Clone)]
pub enum NetworkMessage {
    RequestBlocksByRoot(Vec<H256>),
    SendStatusResponse {
        peer_id: PeerId,
        inbound_request_id: InboundRequestId,
        status: StatusMessage,
    },
    SendBlocksByRootChunk {
        peer_id: PeerId,
        inbound_request_id: InboundRequestId,
        block: Option<Arc<SignedBlockWithAttestation>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncState {
    Idle,
    Syncing,
    Synced,
}

#[derive(Debug, Clone)]
pub struct SyncProgressSnapshot {
    pub sync_state: SyncState,
    pub local_head_slot: Slot,
    pub network_finalized_estimate: Option<Slot>,
    pub connected_peers: usize,
    pub in_flight_requests: usize,
    pub orphan_requests: u64,
    pub backfill_requests: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestState {
    Pending,
    Streaming,
    Completed,
    Failed,
    TimedOut,
}

#[derive(Debug, Clone)]
struct PeerSyncView {
    connected: bool,
    last_status: Option<StatusMessage>,
    in_flight_requests: usize,
}

#[derive(Debug, Clone)]
struct RequestTracker {
    peer_id: PeerId,
    block_roots: Vec<H256>,
    deadline: Instant,
    retries: u8,
    chunks: usize,
    state: RequestState,
}

#[derive(Clone)]
pub struct NetworkService {
    peers: HashMap<PeerId, PeerSyncView>,
    requests: HashMap<AppRequestId, RequestTracker>,
    next_request_id: usize,
    sync_state: SyncState,
    local_head_slot: Slot,
    network_finalized_estimate: Option<Slot>,
    orphan_requests: u64,
    backfill_requests: u64,
    request_timeout: Duration,
    max_retries: u8,
}

impl NetworkService {
    pub fn new() -> Self {
        Self {
            peers: HashMap::new(),
            requests: HashMap::new(),
            next_request_id: 0,
            sync_state: SyncState::Idle,
            local_head_slot: Slot(0),
            network_finalized_estimate: None,
            orphan_requests: 0,
            backfill_requests: 0,
            request_timeout: Duration::from_secs(8),
            max_retries: 2,
        }
    }

    fn peer_view_mut(&mut self, peer_id: PeerId) -> &mut PeerSyncView {
        self.peers.entry(peer_id).or_insert(PeerSyncView {
            connected: true,
            last_status: None,
            in_flight_requests: 0,
        })
    }

    fn choose_connected_peer(&self, exclude: Option<PeerId>) -> Option<PeerId> {
        self.peers
            .iter()
            .filter_map(|(peer_id, state)| {
                if !state.connected || Some(*peer_id) == exclude {
                    return None;
                }
                Some(*peer_id)
            })
            .next()
    }

    fn new_app_request_id(&mut self) -> AppRequestId {
        let id = AppRequestId::Application(self.next_request_id);
        self.next_request_id = self.next_request_id.saturating_add(1);
        id
    }

    fn update_sync_state(&mut self) {
        let connected_peers = self.peers.values().filter(|p| p.connected).count();
        let in_flight = self.in_flight_requests();
        let has_status = self.peers.values().any(|p| p.last_status.is_some());

        let target = if connected_peers == 0 {
            SyncState::Idle
        } else if in_flight > 0 || !has_status {
            SyncState::Syncing
        } else {
            SyncState::Synced
        };

        if Self::is_valid_transition(self.sync_state, target) {
            self.sync_state = target;
        }
    }

    fn is_valid_transition(from: SyncState, to: SyncState) -> bool {
        matches!(
            (from, to),
            (SyncState::Idle, SyncState::Idle)
                | (SyncState::Idle, SyncState::Syncing)
                | (SyncState::Syncing, SyncState::Idle)
                | (SyncState::Syncing, SyncState::Syncing)
                | (SyncState::Syncing, SyncState::Synced)
                | (SyncState::Synced, SyncState::Synced)
                | (SyncState::Synced, SyncState::Syncing)
                | (SyncState::Synced, SyncState::Idle)
        )
    }

    fn in_flight_requests(&self) -> usize {
        self.requests
            .values()
            .filter(|req| matches!(req.state, RequestState::Pending | RequestState::Streaming))
            .count()
    }

    pub fn progress_snapshot(&self) -> SyncProgressSnapshot {
        SyncProgressSnapshot {
            sync_state: self.sync_state,
            local_head_slot: self.local_head_slot,
            network_finalized_estimate: self.network_finalized_estimate,
            connected_peers: self.peers.values().filter(|peer| peer.connected).count(),
            in_flight_requests: self.in_flight_requests(),
            orphan_requests: self.orphan_requests,
            backfill_requests: self.backfill_requests,
        }
    }

    fn handle_request_blocks_by_root(&mut self, block_roots: Vec<H256>) -> ServiceOutput {
        if block_roots.is_empty() {
            return ServiceOutput::none();
        }

        let Some(peer_id) = self.choose_connected_peer(None) else {
            self.backfill_requests = self.backfill_requests.saturating_add(1);
            self.update_sync_state();
            return ServiceOutput::none();
        };

        let app_request_id = self.new_app_request_id();
        self.peer_view_mut(peer_id).in_flight_requests += 1;

        self.requests.insert(
            app_request_id,
            RequestTracker {
                peer_id,
                block_roots: block_roots.clone(),
                deadline: Instant::now() + self.request_timeout,
                retries: 0,
                chunks: 0,
                state: RequestState::Pending,
            },
        );

        self.orphan_requests = self.orphan_requests.saturating_add(1);
        self.update_sync_state();

        ServiceOutput::none().with_effect(Effect::Network(NetworkEffect::SendRequestBlocksByRoot {
            peer_id: Some(peer_id),
            app_request_id,
            block_roots,
        }))
    }

    fn on_rpc_failed(&mut self, peer_id: PeerId, app_request_id: AppRequestId) -> ServiceOutput {
        let mut output = ServiceOutput::none();

        if let Some(peer) = self.peers.get_mut(&peer_id) {
            peer.in_flight_requests = peer.in_flight_requests.saturating_sub(1);
        }

        let retry_plan = if let Some(request) = self.requests.get_mut(&app_request_id) {
            request.state = RequestState::Failed;
            if request.retries < self.max_retries {
                Some((
                    request.retries.saturating_add(1),
                    request.chunks,
                    request.block_roots.clone(),
                ))
            } else {
                None
            }
        } else {
            None
        };

        if let Some((retry_count, chunks, block_roots)) = retry_plan
            && let Some(fallback) = self.choose_connected_peer(Some(peer_id))
        {
            let retry_id = self.new_app_request_id();
            self.requests.insert(
                retry_id,
                RequestTracker {
                    peer_id: fallback,
                    block_roots: block_roots.clone(),
                    deadline: Instant::now() + self.request_timeout,
                    retries: retry_count,
                    chunks,
                    state: RequestState::Pending,
                },
            );

            self.peer_view_mut(fallback).in_flight_requests += 1;

            output = output.with_effect(Effect::Network(NetworkEffect::SendRequestBlocksByRoot {
                peer_id: Some(fallback),
                app_request_id: retry_id,
                block_roots,
            }));
        }

        self.update_sync_state();
        output
    }

    fn check_timeouts(&mut self) -> ServiceOutput {
        let now = Instant::now();
        let mut timed_out = Vec::new();

        for (request_id, request) in &self.requests {
            if matches!(
                request.state,
                RequestState::Pending | RequestState::Streaming
            ) && request.deadline <= now
            {
                timed_out.push(*request_id);
            }
        }

        let mut output = ServiceOutput::none();

        for request_id in timed_out {
            let retry_plan = if let Some(request) = self.requests.get_mut(&request_id) {
                request.state = RequestState::TimedOut;
                if let Some(peer) = self.peers.get_mut(&request.peer_id) {
                    peer.in_flight_requests = peer.in_flight_requests.saturating_sub(1);
                }

                if request.retries >= self.max_retries {
                    None
                } else {
                    Some((
                        request.peer_id,
                        request.retries.saturating_add(1),
                        request.chunks,
                        request.block_roots.clone(),
                    ))
                }
            } else {
                None
            };

            if let Some((failed_peer, retries, chunks, block_roots)) = retry_plan
                && let Some(fallback) = self.choose_connected_peer(Some(failed_peer))
            {
                let retry_id = self.new_app_request_id();
                self.requests.insert(
                    retry_id,
                    RequestTracker {
                        peer_id: fallback,
                        block_roots: block_roots.clone(),
                        deadline: now + self.request_timeout,
                        retries,
                        chunks,
                        state: RequestState::Pending,
                    },
                );

                self.peer_view_mut(fallback).in_flight_requests += 1;
                output = output.with_effect(Effect::Network(NetworkEffect::SendRequestBlocksByRoot {
                    peer_id: Some(fallback),
                    app_request_id: retry_id,
                    block_roots,
                }));
            }
        }

        self.update_sync_state();
        output
    }

    fn on_rpc_response(
        &mut self,
        peer_id: PeerId,
        app_request_id: AppRequestId,
        response: Response,
    ) -> ServiceOutput {
        let mut output = ServiceOutput::none();

        match response {
            Response::Status(status) => {
                let view = self.peer_view_mut(peer_id);
                view.last_status = Some(status);
                self.network_finalized_estimate = Some(status.finalized().slot);

                if let Some(request) = self.requests.get_mut(&app_request_id) {
                    request.state = RequestState::Completed;
                }
            }
            Response::BlocksByRoot(Some(block)) => {
                self.local_head_slot = self.local_head_slot.max(block.message.block.slot);
                if let Some(request) = self.requests.get_mut(&app_request_id) {
                    request.chunks = request.chunks.saturating_add(1);
                    request.state = RequestState::Streaming;
                    request.deadline = Instant::now() + self.request_timeout;
                }

                output = output.with_chain_message(ChainMessage::ProcessNetworkBlock(block));
            }
            Response::BlocksByRoot(None) => {
                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    peer.in_flight_requests = peer.in_flight_requests.saturating_sub(1);
                }

                if let Some(request) = self.requests.get_mut(&app_request_id) {
                    request.state = RequestState::Completed;
                }
            }
        }

        self.update_sync_state();
        output
    }

    fn on_network_event(&mut self, event: NetworkEvent) -> ServiceOutput {
        match event {
            NetworkEvent::GossipBlock(block) => {
                self.local_head_slot = self.local_head_slot.max(block.message.block.slot);
                ServiceOutput::chain_message(ChainMessage::ProcessNetworkBlock(block))
            }
            NetworkEvent::GossipAttestation(attestation) => {
                ServiceOutput::chain_message(ChainMessage::ProcessNetworkAttestation(attestation))
            }
            NetworkEvent::GossipAggregatedAttestation(attestation) => ServiceOutput::chain_message(
                ChainMessage::ProcessNetworkAggregatedAttestation(attestation),
            ),
            NetworkEvent::PeerConnectedIncoming(peer_id)
            | NetworkEvent::PeerConnectedOutgoing(peer_id) => {
                self.peer_view_mut(peer_id).connected = true;
                self.update_sync_state();
                ServiceOutput::none()
            }
            NetworkEvent::PeerDisconnected(peer_id) => {
                if let Some(peer) = self.peers.get_mut(&peer_id) {
                    peer.connected = false;
                    peer.in_flight_requests = 0;
                }
                self.update_sync_state();
                ServiceOutput::none()
            }
            NetworkEvent::StatusPeer(peer_id) => {
                self.peer_view_mut(peer_id).connected = true;
                self.update_sync_state();
                ServiceOutput::none()
            }
            NetworkEvent::RpcStatusRequest {
                peer_id,
                inbound_request_id,
                request,
            } => ServiceOutput::chain_message(ChainMessage::HandleStatusRequest {
                peer_id,
                inbound_request_id,
                request,
            }),
            NetworkEvent::RpcBlocksByRootsRequest {
                peer_id,
                inbound_request_id,
                request,
            } => ServiceOutput::chain_message(ChainMessage::HandleBlocksByRootsRequest {
                peer_id,
                inbound_request_id,
                request,
            }),
            NetworkEvent::RpcResponseReceived {
                peer_id,
                app_request_id,
                response,
            } => self.on_rpc_response(peer_id, app_request_id, response),
            NetworkEvent::RpcFailed {
                peer_id,
                app_request_id,
            } => self.on_rpc_failed(peer_id, app_request_id),
        }
    }
}

impl Service for NetworkService {
    type Message = NetworkMessage;

    fn handle_input(&mut self, input: ServiceInput<Self::Message>) -> ServiceOutput {
        let output = match input {
            ServiceInput::Event(Event::Network(event)) => self.on_network_event(event),
            ServiceInput::Event(Event::Tick(_)) => self.check_timeouts(),
            ServiceInput::Message(NetworkMessage::RequestBlocksByRoot(block_roots)) => {
                self.handle_request_blocks_by_root(block_roots)
            }
            ServiceInput::Message(NetworkMessage::SendStatusResponse {
                peer_id,
                inbound_request_id,
                status,
            }) => ServiceOutput::none().with_effect(Effect::Network(NetworkEffect::SendResponse {
                peer_id,
                inbound_request_id,
                response: Response::Status(status),
            })),
            ServiceInput::Message(NetworkMessage::SendBlocksByRootChunk {
                peer_id,
                inbound_request_id,
                block,
            }) => ServiceOutput::none().with_effect(Effect::Network(NetworkEffect::SendResponse {
                peer_id,
                inbound_request_id,
                response: Response::BlocksByRoot(block),
            })),
        };

        let snapshot = self.progress_snapshot();
        if matches!(snapshot.sync_state, SyncState::Idle) && snapshot.connected_peers > 0 {
            warn!("network service is idle despite connected peers");
        }

        output
    }
}
