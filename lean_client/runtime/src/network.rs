use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::Result;
use containers::{
    ForkDigest, SignedAggregatedAttestation, SignedAttestation, SignedBlockWithAttestation,
};
use fork_choice::ATTESTATION_COMMITTEE_COUNT;
use futures::FutureExt as _;
use libp2p_identity::Keypair;
use networking::{
    AppRequestId, BlocksByRootRequest, EnrForkId, InboundRequestId, Network, NetworkConfig,
    PubsubMessage, RequestType, Response, ServiceContext, StatusMessage, TaskExecutor,
};
use ssz::H256;
use tokio::sync::mpsc;
use tracing::error;

use crate::environment::EventSource;

/// A block or attestation received from the P2P network.
///
/// Emitted by `NetworkEventSource` when a peer gossips a block or attestation,
/// or when a block-by-root response is received.
#[derive(Debug, Clone)]
pub enum NetworkEvent {
    /// A signed block with proposer attestation received from a peer.
    GossipBlock(Arc<SignedBlockWithAttestation>),
    /// A signed attestation received from a peer.
    GossipAttestation(Arc<SignedAttestation>),
    // A signed aggregated attestation received from an aggregator.
    GossipAggregatedAttestation(Arc<SignedAggregatedAttestation>),
    // An incoming status rpc request
    RpcStatusRequest(InboundRequestId, StatusMessage),
    // An incoming blocks by root rpc request
    RpcBlocksByRootsRequest(InboundRequestId, BlocksByRootRequest),
}

/// Network effects consumed by `NetworkEventSource`.
#[derive(Debug, Clone)]
pub enum NetworkEffect {
    /// Gossip a signed block with proposer attestation to the network.
    PublishBlock(Arc<SignedBlockWithAttestation>),

    /// Gossip a signed attestation to the network.
    PublishAttestation(Arc<SignedAttestation>),

    /// Request blocks by root hash from connected peers.
    ///
    /// Emitted by `ChainService` when a received block references an unknown parent.
    /// The `NetworkEventSource` handles this by sending a `BlocksByRoot` request to a peer.
    RequestBlocksByRoot(Vec<H256>),
}

/// Bridges the libp2p P2P stack with the simulation runtime.
///
/// See the [module-level documentation](self) for the design overview.
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

        tokio::spawn(async move {
            let handle_network_event = |event: networking::NetworkEvent| -> Result<()> {
                match event {
                    networking::NetworkEvent::PubsubMessage { message, .. } => match message {
                        PubsubMessage::Attestation(_, attestation) => {
                            event_tx.send(NetworkEvent::GossipAttestation(attestation))?;
                        }
                        PubsubMessage::Block(block) => {
                            event_tx.send(NetworkEvent::GossipBlock(block))?;
                        }
                        PubsubMessage::AggregatedAttestation(att) => {
                            event_tx.send(NetworkEvent::GossipAggregatedAttestation(att))?;
                        }
                    },
                    networking::NetworkEvent::RequestReceived {
                        inbound_request_id,
                        request_type,
                        ..
                    } => match request_type {
                        RequestType::Status(req) => {
                            event_tx.send(NetworkEvent::RpcStatusRequest(inbound_request_id, req))?;
                        }
                        RequestType::BlocksByRoot(req) => {
                            event_tx.send(NetworkEvent::RpcBlocksByRootsRequest(
                                inbound_request_id,
                                req,
                            ))?;
                        }
                    },
                    networking::NetworkEvent::ResponseReceived { response, .. } => {
                        if let Response::BlocksByRoot(Some(block)) = response {
                            event_tx.send(NetworkEvent::GossipBlock(block))?;
                        }
                    }
                    _ => {
                        // currently, we're not interested in any other events.
                    }
                }

                Ok(())
            };

            loop {
                tokio::select! {
                    network_event = network.next_event().fuse() => {
                        if let Err(error) = handle_network_event(network_event) {
                            error!(?error, "failed to process incoming network event");
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
                                let subnet_id =
                                    attestation.validator_id % ATTESTATION_COMMITTEE_COUNT;
                                network
                                    .publish(PubsubMessage::Attestation(subnet_id, attestation));
                            }
                            NetworkEffect::RequestBlocksByRoot(block_roots) => {
                                if block_roots.is_empty() {
                                    continue;
                                }

                                let Some(peer_id) = globals
                                    .peers
                                    .read()
                                    .connected_peer_ids()
                                    .next()
                                    .cloned()
                                else {
                                    error!("no connected peer available for blocks-by-root request");
                                    continue;
                                };

                                let request =
                                    RequestType::BlocksByRoot(BlocksByRootRequest::new(
                                        block_roots.into_iter(),
                                    ));

                                let app_request_id = AppRequestId::Application(
                                    request_id.fetch_add(1, Ordering::Relaxed),
                                );

                                if let Err((_, err)) =
                                    network.send_request(peer_id, app_request_id, request)
                                {
                                    error!(
                                        %peer_id,
                                        ?err,
                                        "failed to send blocks-by-root request",
                                    );
                                }
                            }
                        }
                    }
                }
            }
        });

        Ok(())
    }
}
