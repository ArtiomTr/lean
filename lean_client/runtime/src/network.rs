//! `NetworkEventSource`: bridges the libp2p P2P stack into the simulation framework.
//!
//! ## Design
//!
//! The network is modeled as an [`EventSource`]:
//!
//! - **Events** it emits: blocks and attestations received from peers
//!   (`NetworkEvent::BlockReceived`, `NetworkEvent::AttestationReceived`).
//! - **Effects** it consumes: gossip requests and block-by-root requests from
//!   the service layer (`NetworkEffect::GossipBlock`, `NetworkEffect::GossipAttestation`,
//!   `NetworkEffect::RequestBlocksByRoot`).
//!
//! Internally, `NetworkEventSource` owns the `NetworkService` configuration
//! and spawns three tasks when [`EventSource::run`] is called:
//!
//! 1. **Inbound bridge** – converts the networking layer's inbound messages
//!    (blocks and attestations from the network) into [`NetworkEvent`]s and
//!    forwards them to the simulator via `event_tx`.
//! 2. **Outbound bridge** – converts [`Effect`]s from the simulator into
//!    `OutboundP2pRequest`s and forwards them into the `NetworkService`.
//! 3. **Network task** – builds and runs the `NetworkService` event loop.
//!
//! ## Invariants
//!
//! - [`run`] must be called exactly once. It moves configuration out of `self`
//!   and panics if called a second time.
//! - The `NetworkService` is built inside an async task since its constructor
//!   is async, while [`EventSource::run`] is synchronous.

use std::sync::{Arc, atomic::AtomicU64};

use anyhow::Result;
use containers::{SignedAttestation, SignedBlockWithAttestation};
use libp2p_identity::Keypair;
use ssz::H256;
use networking::{
    service::{NetworkService, NetworkServiceConfig},
    types::{ChainMessage, OutboundP2pRequest},
};
use tokio::sync::mpsc;
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

/// Configuration used to construct the underlying `NetworkService`.
pub struct NetworkConfig {
    /// Network-layer configuration (gossipsub, listen address, ports, bootnodes).
    pub service_config: Arc<NetworkServiceConfig>,
    /// Optional custom libp2p identity keypair.
    ///
    /// When `None`, a random secp256k1 keypair is generated.
    pub keypair: Option<Keypair>,
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

impl EventSource for NetworkEventSource {
    type Event = NetworkEvent;
    type Effect = Effect;

    fn run(
        &mut self,
        event_tx: mpsc::UnboundedSender<NetworkEvent>,
        mut effect_rx: mpsc::UnboundedReceiver<Effect>,
    ) -> Result<()> {
        let NetworkConfig {
            service_config,
            keypair,
        } = self
            .config
            .take()
            .expect("NetworkEventSource::run called more than once");

        // Channel pair that carries inbound messages from the NetworkService.
        // `ChainMessage` is the networking crate's wire type; we convert it to
        // `NetworkEvent` immediately in the inbound bridge below.
        let (chain_tx, mut chain_rx) = mpsc::unbounded_channel::<ChainMessage>();

        // Channel pair that lets the simulation push outbound P2P requests into the
        // NetworkService.  The NetworkService owns `outbound_rx`; the outbound bridge
        // task owns `outbound_tx`.
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<OutboundP2pRequest>();

        // Inbound bridge: convert the networking layer's ChainMessage → NetworkEvent.
        tokio::spawn(async move {
            while let Some(msg) = chain_rx.recv().await {
                let event = match msg {
                    ChainMessage::ProcessBlock {
                        signed_block_with_attestation,
                        ..
                    } => NetworkEvent::BlockReceived(signed_block_with_attestation),
                    ChainMessage::ProcessAttestation {
                        signed_attestation, ..
                    } => NetworkEvent::AttestationReceived(signed_attestation),
                };
                if event_tx.send(event).is_err() {
                    // Simulator has stopped receiving; shut down gracefully.
                    break;
                }
            }
        });

        // Outbound bridge: convert Effects → OutboundP2pRequests.
        tokio::spawn(async move {
            while let Some(effect) = effect_rx.recv().await {
                let Effect::Network(network_effect) = effect;
                let request = match network_effect {
                    NetworkEffect::GossipBlock(block) => OutboundP2pRequest::GossipBlockWithAttestation(block),
                    NetworkEffect::GossipAttestation(att) => OutboundP2pRequest::GossipAttestation(att),
                    NetworkEffect::RequestBlocksByRoot(roots) => OutboundP2pRequest::RequestBlocksByRoot(roots),
                };
                if outbound_tx.send(request).is_err() {
                    // NetworkService has stopped; shut down gracefully.
                    break;
                }
            }
        });

        // Build and run the NetworkService.
        // `NetworkService::new_*` constructors are async, so we must build inside a task.
        tokio::spawn(async move {
            let peer_count = Arc::new(AtomicU64::new(0));

            let result = match keypair {
                Some(kp) => {
                    NetworkService::new_with_keypair(
                        service_config,
                        outbound_rx,
                        chain_tx,
                        peer_count,
                        kp,
                    )
                    .await
                }
                None => {
                    NetworkService::new_with_peer_count(
                        service_config,
                        outbound_rx,
                        chain_tx,
                        peer_count,
                    )
                    .await
                }
            };

            match result {
                Ok(mut svc) => {
                    if let Err(err) = svc.start().await {
                        error!(?err, "Network service exited with error");
                    }
                }
                Err(err) => error!(?err, "Failed to initialize network service"),
            }
        });

        Ok(())
    }
}
