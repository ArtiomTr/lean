use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use anyhow::{Result, anyhow};
use containers::{
    ForkDigest, SignedAggregatedAttestation, SignedAttestation, SignedBlockWithAttestation,
};
use fork_choice::ATTESTATION_COMMITTEE_COUNT;
use futures::FutureExt as _;
use libp2p_identity::Keypair;
use networking::{
    AppRequestId, BlocksByRootRequest, EnrForkId, InboundRequestId, Network, NetworkConfig, PeerId,
    PubsubMessage, RPCError, RequestType, Response, ServiceContext, StatusMessage, TaskExecutor,
};
use ssz::H256;
use tokio::sync::{mpsc, oneshot};
use tracing::{error, warn};

use crate::environment::EventSource;

#[derive(Debug, Clone)]
pub enum NetworkEvent {
    GossipBlock(Arc<SignedBlockWithAttestation>),
    GossipAttestation(Arc<SignedAttestation>),
    GossipAggregatedAttestation(Arc<SignedAggregatedAttestation>),
    PeerConnectedIncoming(PeerId),
    PeerConnectedOutgoing(PeerId),
    PeerDisconnected(PeerId),
    StatusPeer(PeerId),
    RpcStatusRequest {
        peer_id: PeerId,
        inbound_request_id: InboundRequestId,
        request: StatusMessage,
    },
    RpcBlocksByRootsRequest {
        peer_id: PeerId,
        inbound_request_id: InboundRequestId,
        request: BlocksByRootRequest,
    },
    RpcResponseReceived {
        peer_id: PeerId,
        app_request_id: AppRequestId,
        response: Response,
    },
    RpcFailed {
        peer_id: PeerId,
        app_request_id: AppRequestId,
        error: RPCError,
    },
}

#[derive(Debug, Clone)]
pub enum NetworkEffect {
    PublishBlock(Arc<SignedBlockWithAttestation>),
    PublishAttestation(Arc<SignedAttestation>),
    PublishAggregatedAttestation(Arc<SignedAggregatedAttestation>),
    RequestBlocksByRoot(Vec<H256>),
    SendStatusRequest {
        peer_id: PeerId,
        app_request_id: AppRequestId,
        status: StatusMessage,
    },
    SendRequestBlocksByRoot {
        peer_id: Option<PeerId>,
        app_request_id: AppRequestId,
        block_roots: Vec<H256>,
    },
    SendResponse {
        peer_id: PeerId,
        inbound_request_id: InboundRequestId,
        response: Response,
    },
    DisconnectPeer(PeerId),
}

pub struct NetworkEventSource {
    config: Arc<NetworkConfig>,
}

impl NetworkEventSource {
    pub fn new(config: NetworkConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }
}

impl EventSource for NetworkEventSource {
    type Event = NetworkEvent;
    type Effect = NetworkEffect;

    async fn run(
        &mut self,
        event_tx: mpsc::UnboundedSender<Self::Event>,
        mut effect_rx: mpsc::UnboundedReceiver<Self::Effect>,
    ) -> Result<()> {
        let (shutdown_tx, _shutdown_rx) = futures::channel::mpsc::channel(128);

        let executor = TaskExecutor::new(shutdown_tx);
        let context = ServiceContext {
            enr_fork_id: EnrForkId {
                fork_digest: ForkDigest::devnet0(),
                next_fork_version: Default::default(),
                next_fork_epoch: 0,
            },
            config: self.config.clone(),
        };

        let (mut network, globals) =
            Network::new(executor, context, Keypair::generate_ed25519()).await?;
        let request_id = AtomicUsize::new(0);

        loop {
            tokio::select! {
                network_event = network.next_event().fuse() => {
                    if let Err(error) = handle_network_event(network_event, &event_tx) {
                        warn!(?error, "failed to forward network event");
                    }
                }
                effect = effect_rx.recv() => {
                    let Some(effect) = effect else {
                        break;
                    };

                    match effect {
                        NetworkEffect::PublishBlock(block) => {
                            network.publish(PubsubMessage::Block(block));
                        }
                        NetworkEffect::PublishAttestation(attestation) => {
                            let subnet_id = attestation.validator_id % ATTESTATION_COMMITTEE_COUNT;
                            network.publish(PubsubMessage::Attestation(subnet_id, attestation));
                        }
                        NetworkEffect::PublishAggregatedAttestation(attestation) => {
                            network.publish(PubsubMessage::AggregatedAttestation(attestation));
                        }
                        NetworkEffect::RequestBlocksByRoot(block_roots) => {
                            if block_roots.is_empty() {
                                continue;
                            }

                            let Some(peer_id) = globals.peers.read().connected_peer_ids().next().cloned() else {
                                warn!("no connected peer for legacy blocks-by-root request");
                                continue;
                            };

                            let request = RequestType::BlocksByRoot(BlocksByRootRequest::new(block_roots.into_iter()));
                            let app_request_id = AppRequestId::Application(request_id.fetch_add(1, Ordering::Relaxed));
                            if let Err((_, err)) = network.send_request(peer_id, app_request_id, request) {
                                warn!(%peer_id, ?err, "failed to send legacy blocks-by-root request");
                                if let Err(send_err) = event_tx.send(NetworkEvent::RpcFailed {
                                    peer_id,
                                    app_request_id,
                                    error: err,
                                }) {
                                    warn!(?send_err, "failed to forward legacy rpc failure event");
                                }
                            }
                        }
                        NetworkEffect::SendStatusRequest { peer_id, app_request_id, status } => {
                            let request = RequestType::Status(status);
                            if let Err((_, err)) = network.send_request(peer_id, app_request_id, request) {
                                warn!(%peer_id, ?app_request_id, ?err, "failed to send status request");
                                if let Err(send_err) = event_tx.send(NetworkEvent::RpcFailed {
                                    peer_id,
                                    app_request_id,
                                    error: err,
                                }) {
                                    warn!(?send_err, "failed to forward status rpc failure event");
                                }
                            }
                        }
                        NetworkEffect::SendRequestBlocksByRoot { peer_id, app_request_id, block_roots } => {
                            if block_roots.is_empty() {
                                continue;
                            }

                            let chosen_peer = match peer_id {
                                Some(peer) => Some(peer),
                                None => globals.peers.read().connected_peer_ids().next().cloned(),
                            };

                            let Some(peer_id) = chosen_peer else {
                                warn!(?app_request_id, "no connected peer for blocks-by-root request");
                                continue;
                            };

                            let request = RequestType::BlocksByRoot(BlocksByRootRequest::new(block_roots.into_iter()));
                            if let Err((_, err)) = network.send_request(peer_id, app_request_id, request) {
                                warn!(%peer_id, ?app_request_id, ?err, "failed to send blocks-by-root request");
                                if let Err(send_err) = event_tx.send(NetworkEvent::RpcFailed {
                                    peer_id,
                                    app_request_id,
                                    error: err,
                                }) {
                                    warn!(?send_err, "failed to forward blocks-by-root rpc failure event");
                                }
                            }
                        }
                        NetworkEffect::SendResponse { peer_id, inbound_request_id, response } => {
                            network.send_response(peer_id, inbound_request_id, response);
                        }
                        NetworkEffect::DisconnectPeer(peer_id) => {
                            network.__hard_disconnect_testing_only(peer_id);
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

fn handle_network_event(
    event: networking::NetworkEvent,
    event_tx: &mpsc::UnboundedSender<NetworkEvent>,
) -> Result<()> {
    match event {
        networking::NetworkEvent::PubsubMessage { message, .. } => match message {
            PubsubMessage::Attestation(_, attestation) => {
                event_tx.send(NetworkEvent::GossipAttestation(attestation))?;
            }
            PubsubMessage::Block(block) => {
                event_tx.send(NetworkEvent::GossipBlock(block))?;
            }
            PubsubMessage::AggregatedAttestation(attestation) => {
                event_tx.send(NetworkEvent::GossipAggregatedAttestation(attestation))?;
            }
        },
        networking::NetworkEvent::PeerConnectedIncoming(peer_id) => {
            event_tx.send(NetworkEvent::PeerConnectedIncoming(peer_id))?;
        }
        networking::NetworkEvent::PeerConnectedOutgoing(peer_id) => {
            event_tx.send(NetworkEvent::PeerConnectedOutgoing(peer_id))?;
        }
        networking::NetworkEvent::PeerDisconnected(peer_id) => {
            event_tx.send(NetworkEvent::PeerDisconnected(peer_id))?;
        }
        networking::NetworkEvent::StatusPeer(peer_id) => {
            event_tx.send(NetworkEvent::StatusPeer(peer_id))?;
        }
        networking::NetworkEvent::RequestReceived {
            peer_id,
            inbound_request_id,
            request_type,
        } => match request_type {
            RequestType::Status(request) => {
                event_tx.send(NetworkEvent::RpcStatusRequest {
                    peer_id,
                    inbound_request_id,
                    request,
                })?;
            }
            RequestType::BlocksByRoot(request) => {
                event_tx.send(NetworkEvent::RpcBlocksByRootsRequest {
                    peer_id,
                    inbound_request_id,
                    request,
                })?;
            }
        },
        networking::NetworkEvent::ResponseReceived {
            peer_id,
            app_request_id,
            response,
        } => {
            event_tx.send(NetworkEvent::RpcResponseReceived {
                peer_id,
                app_request_id,
                response,
            })?;
        }
        networking::NetworkEvent::RPCFailed {
            app_request_id,
            peer_id,
            error,
        } => {
            error!(%peer_id, ?app_request_id, ?error, "rpc request failed");
            event_tx.send(NetworkEvent::RpcFailed {
                peer_id,
                app_request_id,
                error,
            })?;
        }
        networking::NetworkEvent::NewListenAddr(_)
        | networking::NetworkEvent::ZeroListeners
        | networking::NetworkEvent::PeerUpdatedCustodyGroupCount(_) => {}
    }

    Ok(())
}
