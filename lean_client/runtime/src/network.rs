use std::{
    sync::{Arc, atomic::AtomicU64},
    task::Context,
};

use anyhow::Result;
use containers::{ForkDigest, SignedAttestation, SignedBlockWithAttestation};
use futures::FutureExt as _;
use libp2p_identity::Keypair;
use networking::{EnrForkId, Network, TaskExecutor};
use networking::{NetworkConfig, NetworkEvent as InnerNetworkEvent, ServiceContext};
use ssz::H256;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_stream::wrappers::IntervalStream;
use tracing::error;

use crate::environment::{Effect, EventSource};

/// A block or attestation received from the P2P network.
///
/// Emitted by `NetworkEventSource` when a peer gossips a block or attestation,
/// or when a block-by-root response is received.
#[derive(Debug, Clone)]
pub enum NetworkEvent {
    /// A signed block with proposer attestation received from a peer.
    BlockReceived(SignedBlockWithAttestation),
    /// A signed attestation received from a peer.
    AttestationReceived(SignedAttestation),
}

/// Network effects consumed by `NetworkEventSource`.
#[derive(Debug, Clone)]
pub enum NetworkEffect {
    /// Gossip a signed block with proposer attestation to the network.
    GossipBlock(SignedBlockWithAttestation),

    /// Gossip a signed attestation to the network.
    GossipAttestation(SignedAttestation),

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
    /// Consumed on the first call to [`EventSource::run`].
    config: Option<NetworkConfig>,
}

impl NetworkEventSource {
    pub fn new(config: NetworkConfig) -> Self {
        Self {
            config: Some(config),
        }
    }
}

#[expect(clippy::too_many_lines)]
fn run_network_service(
    mut service: Network,
    mut network_to_service_rx: UnboundedReceiver<ServiceInboundMessage>,
    service_to_network_tx: UnboundedSender<ServiceOutboundMessage>,
) {
    tokio::spawn(async move {
        let mut network_metrics_update_interval =
            IntervalStream::new(tokio::time::interval(NETWORK_METRICS_UPDATE_INTERVAL)).fuse();

        let metrics_enabled = service.network_globals().network_config.metrics_enabled;

        loop {
            tokio::select! {
                _ = network_metrics_update_interval.select_next_some(), if metrics_enabled => {
                    let debug_info = message_debug_info("network_metrics_update_interval");

                    // eth2_libp2p::metrics::update_discovery_metrics();
                    // eth2_libp2p::metrics::update_sync_metrics(service.network_globals());
                    // eth2_libp2p::metrics::update_gossipsub_extended_metrics(
                    //     service.gossipsub(),
                    //     service.network_globals(),
                    // );

                    debug_info.handle();
                },

                network_event = service.next_event().fuse() => {
                    ServiceOutboundMessage::NetworkEvent(network_event).send(&service_to_network_tx);
                }

                message = network_to_service_rx.select_next_some() => {
                    let debug_info = message_debug_info(&message);

                    match message {
                        ServiceInboundMessage::DiscoverSubnetPeers(subnet_discoveries) => {
                            service.discover_subnet_peers(subnet_discoveries);
                        }
                        ServiceInboundMessage::GoodbyePeer(peer_id, goodbye_reason, report_source) => {
                            service.goodbye_peer(&peer_id, goodbye_reason, report_source);
                        }
                        ServiceInboundMessage::Publish(message) => {
                            service.publish(message);
                        }
                        ServiceInboundMessage::ReportPeer(peer_id, action, source, msg) => {
                            service.report_peer(&peer_id, action, source, msg);
                        }
                        ServiceInboundMessage::ReportMessageValidationResult(gossip_id, message_acceptance) => {
                            service.report_message_validation_result(
                                &gossip_id.source,
                                gossip_id.message_id,
                                message_acceptance,
                            );
                        }
                        ServiceInboundMessage::SendErrorResponse(peer_id, inbound_request_id, rpc_error_response, reason) => {
                            service.send_response(
                                peer_id,
                                inbound_request_id,
                                RpcResponse::Error(rpc_error_response, reason.into()),
                            );
                        }
                        ServiceInboundMessage::SendRequest(peer_id, request_id, request) => {
                            if let Err(error) = service.send_request(peer_id, request_id, request) {
                                debug_with_peers!("Unable to send request to peer: {peer_id}: {error:?}");
                            }
                        }
                        ServiceInboundMessage::SendResponse(peer_id, inbound_request_id, response) => {
                            service.send_response(peer_id, inbound_request_id, *response);
                        }
                        ServiceInboundMessage::Subscribe(gossip_topic) => {
                            service.subscribe(gossip_topic);
                        }
                        ServiceInboundMessage::SubscribeKind(gossip_kind) => {
                            service.subscribe_kind(gossip_kind);
                        }
                        ServiceInboundMessage::SubscribeNewForkTopics(phase, fork_digest) => {
                            service.subscribe_new_fork_topics(phase, fork_digest);
                        }
                        ServiceInboundMessage::Unsubscribe(gossip_topic) => {
                            service.unsubscribe(gossip_topic);
                        }
                        ServiceInboundMessage::UnsubscribeFromForkTopicsExcept(fork_digest) => {
                            service.unsubscribe_from_fork_topics_except(fork_digest);
                        }
                        ServiceInboundMessage::UpdateDataColumnSubnets(sampling_size) => {
                            service.subscribe_new_data_column_subnets(sampling_size);
                        }
                        ServiceInboundMessage::UpdateEnrCgc(custody_group_count) => {
                            service.update_enr_cgc(custody_group_count);
                        }
                        ServiceInboundMessage::UpdateEnrSubnet(subnet, advertise) => {
                            service.update_enr_subnet(subnet, advertise);
                        }
                        ServiceInboundMessage::UpdateFork(enr_fork_id) => {
                            service.update_fork_version(enr_fork_id);
                            service.remove_topic_weight_except(enr_fork_id.fork_digest);
                        }
                        ServiceInboundMessage::UpdateGossipsubParameters(active_validator_count, slot) => {
                            if let Err(error) = service.update_gossipsub_parameters(
                                active_validator_count,
                                slot
                            ) {
                                warn_with_peers!("unable to update gossipsub scoring parameters: {error:?}");
                            }
                        }
                        ServiceInboundMessage::UpdateNextForkDigest(next_fork_digest) => {
                            service.update_nfd(next_fork_digest);
                        }
                        ServiceInboundMessage::Stop => break,
                    }

                    debug_info.handle();
                }
            }
        }
    });
}

impl EventSource for NetworkEventSource {
    type Event = NetworkEvent;
    type Effect = NetworkEffect;

    async fn run(
        &mut self,
        event_tx: mpsc::UnboundedSender<Self::Event>,
        mut effect_rx: mpsc::UnboundedReceiver<Self::Effect>,
    ) -> Result<()> {
        let (shutdown_tx, shutdown_rx) = futures::channel::mpsc::channel(128);

        let executor = TaskExecutor::new(shutdown_tx);

        let context = ServiceContext {
            enr_fork_id: EnrForkId {
                fork_digest: ForkDigest::devnet0(),
                next_fork_version: Default::default(),
                next_fork_epoch: 0,
            },
            config: Arc::new(NetworkConfig {
                ..Default::default()
            }),
        };

        let (mut network, globals) =
            Network::new(executor, context, Keypair::generate_ed25519()).await?;

        tokio::spawn(async move {
            fn handle_network_event(event: InnerNetworkEvent) {
                match event {
                    InnerNetworkEvent::PubsubMessage {
                        id,
                        source,
                        topic,
                        message,
                    } => {}
                }
            }

            loop {
                tokio::select! {
                    network_event = network.next_event().fuse() => {

                    }

                    effect = effect_rx.select_next_some() => {

                    }
                }
            }
        });

        Ok(())
    }
}
