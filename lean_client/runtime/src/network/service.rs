use std::{collections::HashMap, sync::Arc, time::Duration};

use containers::{SignedBlockWithAttestation, Slot};
use networking::{AppRequestId, InboundRequestId, PeerId, RPCError, Response, StatusMessage};
use ssz::H256;
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::{
    chain::ChainMessage,
    environment::{Effect, Event, Service, ServiceInput, ServiceOutput},
    network::{NetworkEffect, NetworkEvent},
};

#[derive(Debug, Clone)]
pub enum NetworkMessage {
    RequestBlocksByRoot(Vec<H256>),
    SendStatusRequest {
        peer_id: PeerId,
        status: StatusMessage,
    },
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
    DisconnectPeer(PeerId),
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
}

#[derive(Debug, Clone)]
enum TrackedRequestKind {
    Status,
    BlocksByRoot { block_roots: Vec<H256> },
}

#[derive(Debug, Clone)]
struct PeerSyncView {
    connected: bool,
    last_status: Option<StatusMessage>,
    in_flight_requests: usize,
    successful_requests: u64,
    failed_requests: u64,
}

#[derive(Debug, Clone)]
struct RequestTracker {
    peer_id: PeerId,
    kind: TrackedRequestKind,
    created_at: Instant,
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
    max_in_flight_per_peer: usize,
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
            max_in_flight_per_peer: 2,
        }
    }

    fn peer_view_mut(&mut self, peer_id: PeerId) -> &mut PeerSyncView {
        self.peers.entry(peer_id).or_insert(PeerSyncView {
            connected: true,
            last_status: None,
            in_flight_requests: 0,
            successful_requests: 0,
            failed_requests: 0,
        })
    }

    fn choose_connected_peer(&self, exclude: Option<PeerId>) -> Option<PeerId> {
        let mut candidates = self
            .peers
            .iter()
            .filter_map(|(peer_id, state)| {
                if !state.connected
                    || Some(*peer_id) == exclude
                    || state.in_flight_requests >= self.max_in_flight_per_peer
                {
                    return None;
                }

                let finalized_slot = state
                    .last_status
                    .map_or(0, |status| status.finalized().slot.0);
                let head_slot = state.last_status.map_or(0, |status| status.head().slot.0);

                Some((
                    *peer_id,
                    state.in_flight_requests,
                    finalized_slot,
                    head_slot,
                ))
            })
            .collect::<Vec<_>>();

        candidates.sort_by(|left, right| {
            left.1
                .cmp(&right.1)
                .then_with(|| right.2.cmp(&left.2))
                .then_with(|| right.3.cmp(&left.3))
                .then_with(|| left.0.to_bytes().cmp(&right.0.to_bytes()))
        });

        candidates.into_iter().next().map(|candidate| candidate.0)
    }

    fn new_app_request_id(&mut self) -> Option<AppRequestId> {
        let max_probe = self.requests.len().saturating_add(1).max(1024);

        for _ in 0..max_probe {
            let next = self.next_request_id;
            self.next_request_id = self.next_request_id.wrapping_add(1);

            let candidate = AppRequestId::Application(next);
            if !self.requests.contains_key(&candidate) {
                return Some(candidate);
            }
        }

        None
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

    fn issue_blocks_by_root_request(
        &mut self,
        peer_id: PeerId,
        block_roots: Vec<H256>,
        retries: u8,
        chunks: usize,
    ) -> Option<ServiceOutput> {
        let Some(app_request_id) = self.new_app_request_id() else {
            warn!("request id allocation exhausted; delaying blocks-by-root request");
            return None;
        };

        let now = Instant::now();
        self.requests.insert(
            app_request_id,
            RequestTracker {
                peer_id,
                kind: TrackedRequestKind::BlocksByRoot {
                    block_roots: block_roots.clone(),
                },
                created_at: now,
                deadline: now + self.request_timeout,
                retries,
                chunks,
                state: RequestState::Pending,
            },
        );

        self.peer_view_mut(peer_id).in_flight_requests += 1;

        Some(ServiceOutput::none().with_effect(Effect::Network(
            NetworkEffect::SendRequestBlocksByRoot {
                peer_id: Some(peer_id),
                app_request_id,
                block_roots,
            },
        )))
    }

    fn issue_status_request(
        &mut self,
        peer_id: PeerId,
        status: StatusMessage,
    ) -> Option<ServiceOutput> {
        let Some(app_request_id) = self.new_app_request_id() else {
            warn!("request id allocation exhausted; delaying status request");
            return None;
        };

        let now = Instant::now();
        self.requests.insert(
            app_request_id,
            RequestTracker {
                peer_id,
                kind: TrackedRequestKind::Status,
                created_at: now,
                deadline: now + self.request_timeout,
                retries: 0,
                chunks: 0,
                state: RequestState::Pending,
            },
        );

        self.peer_view_mut(peer_id).in_flight_requests += 1;

        Some(
            ServiceOutput::none().with_effect(Effect::Network(NetworkEffect::SendStatusRequest {
                peer_id,
                app_request_id,
                status,
            })),
        )
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

        self.orphan_requests = self.orphan_requests.saturating_add(1);

        let output = self
            .issue_blocks_by_root_request(peer_id, block_roots, 0, 0)
            .unwrap_or_else(ServiceOutput::none);

        self.update_sync_state();
        output
    }

    fn retry_failed_request(
        &mut self,
        failed_peer: PeerId,
        retries: u8,
        chunks: usize,
        kind: TrackedRequestKind,
    ) -> ServiceOutput {
        if retries > self.max_retries {
            return ServiceOutput::none();
        }

        let mut output = ServiceOutput::none();

        if let Some(fallback) = self.choose_connected_peer(Some(failed_peer)) {
            match kind {
                TrackedRequestKind::Status => {
                    debug!(
                        %failed_peer,
                        %fallback,
                        retries,
                        "retrying status request against fallback peer"
                    );

                    output = output
                        .with_chain_message(ChainMessage::SendStatusRequest { peer_id: fallback });
                }
                TrackedRequestKind::BlocksByRoot { block_roots } => {
                    if let Some(retry_output) =
                        self.issue_blocks_by_root_request(fallback, block_roots, retries, chunks)
                    {
                        output.messages.extend(retry_output.messages);
                        output.effects.extend(retry_output.effects);
                    }
                }
            }
        } else {
            warn!(%failed_peer, retries, "no fallback peer available for retry");
            output =
                output.with_effect(Effect::Network(NetworkEffect::DisconnectPeer(failed_peer)));
        }

        output
    }

    fn on_rpc_failed(
        &mut self,
        peer_id: PeerId,
        app_request_id: AppRequestId,
        error: RPCError,
    ) -> ServiceOutput {
        let mut output = ServiceOutput::none();

        let Some(request) = self.requests.remove(&app_request_id) else {
            warn!(%peer_id, ?app_request_id, ?error, "rpc failed for unknown request id");
            self.update_sync_state();
            return output;
        };

        if request.peer_id != peer_id {
            warn!(
                expected_peer = %request.peer_id,
                got_peer = %peer_id,
                ?app_request_id,
                "rpc failure peer mismatch"
            );
        }

        if let Some(peer) = self.peers.get_mut(&request.peer_id) {
            peer.in_flight_requests = peer.in_flight_requests.saturating_sub(1);
            peer.failed_requests = peer.failed_requests.saturating_add(1);
        }

        warn!(
            %peer_id,
            ?app_request_id,
            ?error,
            retries = request.retries,
            request_age_ms = request.created_at.elapsed().as_millis(),
            "rpc request failed"
        );

        let next_retry = request.retries.saturating_add(1);
        let retry_output =
            self.retry_failed_request(request.peer_id, next_retry, request.chunks, request.kind);
        output.messages.extend(retry_output.messages);
        output.effects.extend(retry_output.effects);

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
            let Some(request) = self.requests.remove(&request_id) else {
                continue;
            };

            if let Some(peer) = self.peers.get_mut(&request.peer_id) {
                peer.in_flight_requests = peer.in_flight_requests.saturating_sub(1);
                peer.failed_requests = peer.failed_requests.saturating_add(1);
            }

            let next_retry = request.retries.saturating_add(1);
            let retry_output = self.retry_failed_request(
                request.peer_id,
                next_retry,
                request.chunks,
                request.kind,
            );
            output.messages.extend(retry_output.messages);
            output.effects.extend(retry_output.effects);
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
                let finalized_slot = status.finalized().slot;
                let view = self.peer_view_mut(peer_id);
                view.last_status = Some(status);
                self.network_finalized_estimate = Some(finalized_slot);

                if let Some(request) = self.requests.remove(&app_request_id) {
                    if let Some(peer) = self.peers.get_mut(&request.peer_id) {
                        peer.in_flight_requests = peer.in_flight_requests.saturating_sub(1);
                        peer.successful_requests = peer.successful_requests.saturating_add(1);
                    }
                }
            }
            Response::BlocksByRoot(Some(block)) => {
                self.local_head_slot = self.local_head_slot.max(block.message.block.slot);

                if let Some(request) = self.requests.get_mut(&app_request_id) {
                    if request.peer_id != peer_id {
                        warn!(
                            expected_peer = %request.peer_id,
                            got_peer = %peer_id,
                            ?app_request_id,
                            "ignoring blocks-by-root chunk from unexpected peer"
                        );
                        self.update_sync_state();
                        return output;
                    }

                    request.chunks = request.chunks.saturating_add(1);
                    request.state = RequestState::Streaming;
                    request.deadline = Instant::now() + self.request_timeout;
                } else {
                    warn!(%peer_id, ?app_request_id, "received blocks-by-root chunk for unknown request");
                }

                output = output.with_chain_message(ChainMessage::ProcessNetworkBlock(block));
            }
            Response::BlocksByRoot(None) => {
                let Some(request) = self.requests.remove(&app_request_id) else {
                    warn!(%peer_id, ?app_request_id, "received stream termination for unknown request");
                    self.update_sync_state();
                    return output;
                };

                if let Some(peer) = self.peers.get_mut(&request.peer_id) {
                    peer.in_flight_requests = peer.in_flight_requests.saturating_sub(1);
                    peer.successful_requests = peer.successful_requests.saturating_add(1);
                }

                if request.chunks == 0 {
                    debug!(
                        %peer_id,
                        ?app_request_id,
                        "blocks-by-root request ended with no payload chunks"
                    );
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

                self.requests
                    .retain(|_, request| request.peer_id != peer_id);

                self.update_sync_state();
                ServiceOutput::none()
            }
            NetworkEvent::StatusPeer(peer_id) => {
                self.peer_view_mut(peer_id).connected = true;
                self.update_sync_state();
                ServiceOutput::chain_message(ChainMessage::SendStatusRequest { peer_id })
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
                error,
            } => self.on_rpc_failed(peer_id, app_request_id, error),
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
            ServiceInput::Message(NetworkMessage::SendStatusRequest { peer_id, status }) => self
                .issue_status_request(peer_id, status)
                .unwrap_or_else(ServiceOutput::none),
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
            ServiceInput::Message(NetworkMessage::DisconnectPeer(peer_id)) => ServiceOutput::none()
                .with_effect(Effect::Network(NetworkEffect::DisconnectPeer(peer_id))),
        };

        let snapshot = self.progress_snapshot();
        if matches!(snapshot.sync_state, SyncState::Idle) && snapshot.connected_peers > 0 {
            warn!("network service is idle despite connected peers");
        }

        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_id_rollover_skips_in_flight_ids() {
        let mut service = NetworkService::new();
        service.next_request_id = usize::MAX;

        let peer = PeerId::random();
        service.peer_view_mut(peer).connected = true;

        let first = service
            .issue_blocks_by_root_request(peer, vec![H256::zero()], 0, 0)
            .expect("request should be allocated");
        assert_eq!(first.effects.len(), 1);

        let second = service
            .issue_blocks_by_root_request(peer, vec![H256::zero()], 0, 0)
            .expect("request should be allocated after rollover");
        assert_eq!(second.effects.len(), 1);
        assert_eq!(service.requests.len(), 2);
    }

    #[test]
    fn rpc_failure_retries_on_fallback_peer() {
        let mut service = NetworkService::new();
        let peer_a = PeerId::random();
        let peer_b = PeerId::random();
        service.peer_view_mut(peer_a).connected = true;
        service.peer_view_mut(peer_b).connected = true;

        let output = service.handle_input(ServiceInput::Message(
            NetworkMessage::RequestBlocksByRoot(vec![H256::zero()]),
        ));
        assert_eq!(output.effects.len(), 1);

        let (request_id, original_peer) = service
            .requests
            .iter()
            .next()
            .map(|(id, req)| (*id, req.peer_id))
            .expect("request tracker should exist");

        let fail_output = service.handle_input(ServiceInput::Event(Event::Network(
            NetworkEvent::RpcFailed {
                peer_id: original_peer,
                app_request_id: request_id,
                error: RPCError::Disconnected,
            },
        )));

        assert!(fail_output.effects.iter().any(|effect| matches!(
            effect,
            Effect::Network(NetworkEffect::SendRequestBlocksByRoot { .. })
        )));
    }
}
