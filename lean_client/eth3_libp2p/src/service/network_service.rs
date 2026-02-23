use crate::NetworkConfig;
use crate::service::Network;
use crate::service::utils::load_private_key;
use crate::service::utils::Context as ServiceContext;
use crate::task_executor::{ShutdownReason, TaskExecutor};
use crate::types::{ChainMessage, EnrForkId, ForkContext, OutboundP2pRequest};
use crate::types::pubsub::PubsubMessage;
use anyhow::Result;
use futures::channel::mpsc as futures_mpsc;
use libp2p::identity::Keypair;
use ssz::H256;
use std::sync::{Arc, atomic::AtomicU64};
use tokio::sync::mpsc;
use types::config::Config as ChainConfig;

/// Configuration for the NetworkService.
pub struct NetworkServiceConfig {
    pub chain_config: Arc<ChainConfig>,
    pub network_config: Arc<NetworkConfig>,
}

/// High-level network service that bridges the P2P stack with the application layer.
pub struct NetworkService {
    network: Network,
    chain_tx: mpsc::UnboundedSender<ChainMessage>,
    outbound_rx: mpsc::UnboundedReceiver<OutboundP2pRequest>,
    peer_count: Arc<AtomicU64>,
}

impl NetworkService {
    /// Create a new NetworkService with the given keypair.
    pub async fn new_with_keypair(
        service_config: Arc<NetworkServiceConfig>,
        outbound_rx: mpsc::UnboundedReceiver<OutboundP2pRequest>,
        chain_tx: mpsc::UnboundedSender<ChainMessage>,
        peer_count: Arc<AtomicU64>,
        keypair: Keypair,
    ) -> Result<Self> {
        let chain_config = service_config.chain_config.clone();
        let network_config = service_config.network_config.clone();

        let fork_context = Arc::new(ForkContext::new(&chain_config, H256::default()));
        let enr_fork_id = EnrForkId {
            fork_digest: fork_context.current_fork_digest(),
            ..EnrForkId::default()
        };

        let ctx = ServiceContext {
            chain_config: chain_config.clone(),
            config: network_config,
            enr_fork_id,
            fork_context,
        };

        // Create a throwaway shutdown channel; the receiver is dropped immediately
        // since we don't need shutdown signalling at this layer.
        let (signal_tx, _signal_rx) = futures_mpsc::channel::<ShutdownReason>(1);
        let executor = TaskExecutor::new(signal_tx);

        let (network, _globals) = Network::new(
            chain_config,
            executor,
            ctx,
            keypair,
        )
        .await?;

        Ok(Self {
            network,
            chain_tx,
            outbound_rx,
            peer_count,
        })
    }

    /// Create a new NetworkService, loading or generating a keypair from the network config.
    pub async fn new_with_peer_count(
        service_config: Arc<NetworkServiceConfig>,
        outbound_rx: mpsc::UnboundedReceiver<OutboundP2pRequest>,
        chain_tx: mpsc::UnboundedSender<ChainMessage>,
        peer_count: Arc<AtomicU64>,
    ) -> Result<Self> {
        let keypair = load_private_key(&service_config.network_config);
        Self::new_with_keypair(service_config, outbound_rx, chain_tx, peer_count, keypair).await
    }

    /// Run the network event loop.
    pub async fn start(&mut self) -> Result<()> {
        use crate::service::NetworkEvent;
        use std::sync::atomic::Ordering;

        loop {
            tokio::select! {
                event = self.network.next_event() => {
                    match event {
                        NetworkEvent::PubsubMessage { message, .. } => {
                            let chain_msg = match message {
                                PubsubMessage::Block(block) => {
                                    Some(ChainMessage::ProcessBlock {
                                        signed_block_with_attestation: (*block).clone(),
                                    })
                                }
                                PubsubMessage::Attestation(_subnet_id, attestation) => {
                                    Some(ChainMessage::ProcessAttestation {
                                        signed_attestation: (*attestation).clone(),
                                    })
                                }
                                _ => None,
                            };
                            if let Some(msg) = chain_msg {
                                let _ = self.chain_tx.send(msg);
                            }
                        }
                        NetworkEvent::PeerConnectedOutgoing(_)
                        | NetworkEvent::PeerConnectedIncoming(_) => {
                            self.peer_count.fetch_add(1, Ordering::Relaxed);
                        }
                        NetworkEvent::PeerDisconnected(_) => {
                            self.peer_count.fetch_sub(1, Ordering::Relaxed);
                        }
                        _ => {}
                    }
                }
                Some(request) = self.outbound_rx.recv() => {
                    match request {
                        OutboundP2pRequest::GossipBlockWithAttestation(block) => {
                            self.network.publish(PubsubMessage::Block(Arc::new(block)));
                        }
                        OutboundP2pRequest::GossipAttestation(attestation) => {
                            self.network.publish(PubsubMessage::Attestation(0, Arc::new(attestation)));
                        }
                        OutboundP2pRequest::RequestBlocksByRoot(_roots) => {
                            // TODO: implement block requests
                        }
                    }
                }
            }
        }
    }
}
