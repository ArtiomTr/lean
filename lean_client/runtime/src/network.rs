//! `NetworkEventSource`: bridges the libp2p P2P stack into the simulation framework.
//!
//! ## Design
//!
//! The network is modeled as an [`EventSource`]:
//!
//! - **Events** it emits: blocks and attestations received from peers
//!   (`NetworkEvent::BlockReceived`, `NetworkEvent::AttestationReceived`).
//! - **Effects** it consumes: gossip requests and block-by-root requests from
//!   the service layer (`Effect::GossipBlock`, `Effect::GossipAttestation`,
//!   `Effect::RequestBlocksByRoot`).
//!
//! Internally, `NetworkEventSource` owns the `NetworkService` configuration
//! and spawns three tasks when [`EventSource::run`] is called:
//!
//! 1. **Network task** – builds and runs the `NetworkService` event loop.
//! 2. **Inbound bridge** – converts `networking::types::ChainMessage` (blocks
//!    and attestations from the network) into [`NetworkEvent`]s and forwards
//!    them to the simulator via `event_tx`.
//! 3. **Outbound bridge** – converts [`Effect`]s from the simulator into
//!    `OutboundP2pRequest`s and forwards them into the `NetworkService`.
//!
//! ## Invariants
//!
//! - [`run`] must be called exactly once. It moves configuration out of `self`
//!   and panics if called a second time.
//! - The `NetworkService` is built inside an async task since its constructor
//!   is async, while [`EventSource::run`] is synchronous.

use std::sync::{Arc, atomic::AtomicU64};

use anyhow::Result;
use libp2p_identity::Keypair;
use networking::{
    service::{NetworkService, NetworkServiceConfig},
    types::{ChainMessage as NetworkChainMessage, OutboundP2pRequest},
};
use tokio::sync::mpsc;
use tracing::error;

use crate::simulation::{Effect, EventSource, NetworkEvent};

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
        effect_rx: mpsc::UnboundedReceiver<Effect>,
    ) -> Result<()> {
        let NetworkConfig {
            service_config,
            keypair,
        } = self
            .config
            .take()
            .expect("NetworkEventSource::run called more than once");

        // Channel pair that lets the simulation push outbound P2P requests into the
        // NetworkService.  The NetworkService owns `outbound_rx`; the outbound bridge
        // task owns `outbound_tx`.
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<OutboundP2pRequest>();

        // Channel pair that carries inbound messages (blocks / attestations) from the
        // NetworkService to the inbound bridge task.
        let (chain_msg_tx, chain_msg_rx) = mpsc::unbounded_channel::<NetworkChainMessage>();

        // Spawn the inbound bridge: convert NetworkChainMessages → NetworkEvents.
        spawn_inbound_bridge(chain_msg_rx, event_tx);

        // Spawn the outbound bridge: convert Effects → OutboundP2pRequests.
        spawn_outbound_bridge(effect_rx, outbound_tx);

        // Spawn the NetworkService itself.
        spawn_network_service(service_config, keypair, outbound_rx, chain_msg_tx);

        Ok(())
    }
}

// ── Private helpers ──────────────────────────────────────────────────────────

/// Forwards inbound `NetworkChainMessage`s as `NetworkEvent`s into the simulator.
fn spawn_inbound_bridge(
    mut chain_msg_rx: mpsc::UnboundedReceiver<NetworkChainMessage>,
    event_tx: mpsc::UnboundedSender<NetworkEvent>,
) {
    tokio::spawn(async move {
        while let Some(msg) = chain_msg_rx.recv().await {
            let network_event = match msg {
                NetworkChainMessage::ProcessBlock {
                    signed_block_with_attestation,
                    ..
                } => NetworkEvent::BlockReceived(signed_block_with_attestation),

                NetworkChainMessage::ProcessAttestation {
                    signed_attestation, ..
                } => NetworkEvent::AttestationReceived(signed_attestation),
            };

            if event_tx.send(network_event).is_err() {
                // Simulator has stopped receiving; shut down gracefully.
                break;
            }
        }
    });
}

/// Forwards simulation `Effect`s as `OutboundP2pRequest`s into the `NetworkService`.
fn spawn_outbound_bridge(
    mut effect_rx: mpsc::UnboundedReceiver<Effect>,
    outbound_tx: mpsc::UnboundedSender<OutboundP2pRequest>,
) {
    tokio::spawn(async move {
        while let Some(effect) = effect_rx.recv().await {
            let request = match effect {
                Effect::GossipBlock(block) => OutboundP2pRequest::GossipBlockWithAttestation(block),
                Effect::GossipAttestation(att) => OutboundP2pRequest::GossipAttestation(att),
                Effect::RequestBlocksByRoot(roots) => {
                    OutboundP2pRequest::RequestBlocksByRoot(roots)
                }
            };

            if outbound_tx.send(request).is_err() {
                // NetworkService has stopped; shut down gracefully.
                break;
            }
        }
    });
}

/// Builds and runs the `NetworkService` in a background task.
fn spawn_network_service(
    service_config: Arc<NetworkServiceConfig>,
    keypair: Option<Keypair>,
    outbound_rx: mpsc::UnboundedReceiver<OutboundP2pRequest>,
    chain_msg_tx: mpsc::UnboundedSender<NetworkChainMessage>,
) {
    // `NetworkService::new_*` constructors are async, so we must build inside a task.
    tokio::spawn(async move {
        // A shared peer counter — currently informational only inside the runtime.
        let peer_count = Arc::new(AtomicU64::new(0));

        let result = match keypair {
            Some(kp) => {
                NetworkService::new_with_keypair(
                    service_config,
                    outbound_rx,
                    chain_msg_tx,
                    peer_count,
                    kp,
                )
                .await
            }
            None => {
                NetworkService::new_with_peer_count(
                    service_config,
                    outbound_rx,
                    chain_msg_tx,
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
}
