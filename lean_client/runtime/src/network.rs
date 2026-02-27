use std::{
    sync::{Arc, atomic::AtomicU64},
    task::Context,
};

use anyhow::Result;
use containers::{
    AggregatedAttestation, ForkDigest, SignedAggregatedAttestation, SignedAttestation,
    SignedBlockWithAttestation,
};
use futures::FutureExt as _;
use libp2p_identity::Keypair;
use networking::{
    BlocksByRootRequest, EnrForkId, InboundRequestId, Network, PubsubMessage, RequestType,
    StatusMessage, TaskExecutor,
};
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
        let (shutdown_tx, shutdown_rx) = futures::channel::mpsc::channel(128);

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

        tokio::spawn(async move {
            let handle_network_event = |event: InnerNetworkEvent| -> Result<()> {
                match event {
                    InnerNetworkEvent::PubsubMessage { message, .. } => match message {
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
                    InnerNetworkEvent::RequestReceived {
                        inbound_request_id,
                        request_type,
                        ..
                    } => match request_type {
                        RequestType::Status(req) => {
                            event_tx.send(NetworkEvent::RpcStatusRequest(inbound_request_id, req));
                        }
                        RequestType::BlocksByRoot(req) => {
                            event_tx.send(NetworkEvent::RpcBlocksByRootsRequest(
                                inbound_request_id,
                                req,
                            ))?;
                        }
                    },
                    _ => {
                        // currently, we're not interested in any other events.
                    }
                }

                Ok(())
            };

            loop {
                tokio::select! {
                    network_event = network.next_event().fuse() => {
                        let _ = handle_network_event(network_event);
                    }

                    effect = effect_rx.recv() => {

                    }
                }
            }
        });

        Ok(())
    }
}
