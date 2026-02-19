pub mod api_types;
pub mod behaviour;

use std::{
    collections::HashMap,
    fs::File,
    io,
    num::{NonZeroU8, NonZeroUsize},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Result, anyhow};
use derive_more::Display;
use discv5::Enr;
use futures::StreamExt;
use libp2p::{
    Multiaddr, SwarmBuilder,
    connection_limits::{self, ConnectionLimits},
    gossipsub::{Event as GossipsubEvent, IdentTopic, MessageAuthenticity},
    identify,
    multiaddr::Protocol,
    swarm::{Config as SwarmConfig, ConnectionError, Swarm, SwarmEvent},
};
use libp2p_identity::{Keypair, PeerId};
use metrics::{DisconnectReason, METRICS};
use parking_lot::Mutex;
use rand::seq::IndexedRandom;
use serde::{Deserialize, Serialize};
use ssz::{H256, SszWrite as _};
use tokio::select;
use tokio::time::{Duration, MissedTickBehavior, interval};
use tracing::{debug, info, trace, warn};

use crate::{
    bootnodes::{Bootnode as CrateBootnode, BootnodeSource},
    config::{Config, GossipsubConfig},
    discovery::{DiscoveryConfig, DiscoveryService},
    rpc::{self, LeanRequest, LeanResponse, RpcEvent},
    types::{
        ChainMessage, ChainMessageSink, ConnectionState, OutboundP2pRequest, P2pRequestSource,
        pubsub::PubsubMessage, topics::GossipKind,
    },
};

use crate::discovery::enr_ext::EnrExt as _;
use crate::service::behaviour::{Behaviour, BehaviourEvent, LeanGossipsub};

pub mod config {
    pub use crate::config::{Config, GossipsubConfig};
}

// ── Bootnode parsing ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Display)]
#[serde(untagged)]
enum BootnodeArg {
    Multiaddr(Multiaddr),
    Enr(Enr),
}

impl BootnodeArg {
    fn addrs(&self) -> Vec<Multiaddr> {
        match self {
            Self::Multiaddr(addr) => vec![addr.clone()],
            Self::Enr(enr) => enr.multiaddr_quic(),
        }
    }
}

/// Parse a single bootnode argument string into zero or more `Bootnode` values.
///
/// The argument may be:
/// - A multiaddr string (e.g. `/ip4/1.2.3.4/udp/9000/quic-v1/p2p/12D3…`)
/// - An ENR string (e.g. `enr:-…`)
/// - A path to a YAML file containing a list of the above
pub fn parse_bootnode_arg(arg: &str) -> Vec<CrateBootnode> {
    if let Ok(addr) = arg.parse::<Multiaddr>() {
        return vec![CrateBootnode::Multiaddr(addr)];
    }

    if let Ok(enr) = arg.parse::<Enr>() {
        return vec![CrateBootnode::Enr(enr)];
    }

    let Some(file) = File::open(arg).ok() else {
        warn!(
            "value {arg:?} provided as bootnode is not recognized - it is not valid multiaddr nor valid path to file containing bootnodes."
        );
        return Vec::new();
    };

    let raw_bootnodes: Vec<BootnodeArg> = match serde_yaml::from_reader(file) {
        Ok(value) => value,
        Err(err) => {
            warn!("failed to read bootnodes from {arg:?}: {err:?}");
            return Vec::new();
        }
    };

    if raw_bootnodes.is_empty() {
        warn!("provided file with bootnodes {arg:?} is empty");
    }

    raw_bootnodes
        .into_iter()
        .map(|b| match b {
            BootnodeArg::Multiaddr(addr) => CrateBootnode::Multiaddr(addr),
            BootnodeArg::Enr(enr) => CrateBootnode::Enr(enr),
        })
        .collect()
}

// ── NetworkService ───────────────────────────────────────────────────────────

/// Lean-ethereum network service configuration.
///
/// This is the legacy config type kept for backward compatibility with
/// callers that still use `NetworkServiceConfig`. New code should use `Config`.
pub use crate::config::Config as NetworkServiceConfig;

#[derive(Debug)]
pub enum NetworkEvent {
    PeerConnectedIncoming(PeerId),
    PeerConnectedOutgoing(PeerId),
    PeerDisconnected(PeerId),
    Status(PeerId),
    Ping(PeerId),
    MetaData(PeerId),
    DisconnectPeer(PeerId),
}

/// The main lean ethereum network service.
///
/// Mirrors eth2_libp2p's `Network` (formerly NetworkService) struct.
/// Owns the libp2p Swarm, discovery service, and message routing.
pub struct NetworkService<R, S>
where
    R: P2pRequestSource<OutboundP2pRequest> + Send + 'static,
    S: ChainMessageSink<ChainMessage> + Send + 'static,
{
    network_config: Arc<NetworkServiceConfig>,
    swarm: Swarm<Behaviour>,
    discovery: Option<DiscoveryService>,
    peer_table: Arc<Mutex<HashMap<PeerId, ConnectionState>>>,
    peer_count: Arc<AtomicU64>,
    outbound_p2p_requests: R,
    chain_message_sink: S,
}

impl<R, S> NetworkService<R, S>
where
    R: P2pRequestSource<OutboundP2pRequest> + Send + 'static,
    S: ChainMessageSink<ChainMessage> + Send + 'static,
{
    pub async fn new(
        network_config: Arc<NetworkServiceConfig>,
        outbound_p2p_requests: R,
        chain_message_sink: S,
    ) -> Result<Self> {
        Self::new_with_peer_count(
            network_config,
            outbound_p2p_requests,
            chain_message_sink,
            Arc::new(AtomicU64::new(0)),
        )
        .await
    }

    pub async fn new_with_peer_count(
        network_config: Arc<NetworkServiceConfig>,
        outbound_p2p_requests: R,
        chain_message_sink: S,
        peer_count: Arc<AtomicU64>,
    ) -> Result<Self> {
        let local_key = Keypair::generate_secp256k1();
        Self::new_with_keypair(
            network_config,
            outbound_p2p_requests,
            chain_message_sink,
            peer_count,
            local_key,
        )
        .await
    }

    pub async fn new_with_keypair(
        network_config: Arc<NetworkServiceConfig>,
        outbound_p2p_requests: R,
        chain_message_sink: S,
        peer_count: Arc<AtomicU64>,
        local_key: Keypair,
    ) -> Result<Self> {
        let behaviour = Self::build_behaviour(&local_key, &network_config)?;

        let config = SwarmConfig::with_tokio_executor()
            .with_notify_handler_buffer_size(NonZeroUsize::new(7).unwrap())
            .with_per_connection_event_buffer_size(4)
            .with_dial_concurrency_factor(NonZeroU8::new(1).unwrap());

        let multiaddr = Self::multiaddr(&network_config)?;
        let swarm = SwarmBuilder::with_existing_identity(local_key.clone())
            .with_tokio()
            .with_quic()
            .with_behaviour(|_| behaviour)?
            .with_swarm_config(|_| config)
            .build();

        let discovery = if network_config.discovery_enabled {
            let discovery_config = DiscoveryConfig::new(
                network_config.socket_address,
                network_config.discovery_port,
                network_config.socket_port,
            )
            .with_bootnodes(network_config.enr_bootnodes());

            match DiscoveryService::new(discovery_config, &local_key).await {
                Ok(disc) => {
                    info!(
                        enr = %disc.local_enr(),
                        "Discovery service initialized"
                    );
                    Some(disc)
                }
                Err(e) => {
                    warn!(error = ?e, "Failed to initialize discovery service, continuing without it");
                    None
                }
            }
        } else {
            info!("Discovery service disabled");
            None
        };

        let mut service = Self {
            network_config,
            swarm,
            discovery,
            peer_table: Arc::new(Mutex::new(HashMap::new())),
            peer_count,
            outbound_p2p_requests,
            chain_message_sink,
        };

        service.listen(&multiaddr)?;
        service.subscribe_to_topics()?;

        Ok(service)
    }

    pub async fn start(&mut self) -> Result<()> {
        let mut reconnect_interval = interval(Duration::from_secs(30));
        reconnect_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let mut discovery_interval = interval(Duration::from_secs(30));
        discovery_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            select! {
                _ = reconnect_interval.tick() => {
                    self.connect_to_peers(self.network_config.to_multiaddrs()).await;
                }
                _ = discovery_interval.tick() => {
                    if let Some(ref discovery) = self.discovery {
                        let known_peers = discovery.connected_peers();
                        debug!(known_peers, "Triggering random peer discovery lookup");
                        discovery.find_random_peers();
                    }
                }
                request = self.outbound_p2p_requests.recv() => {
                    if let Some(request) = request {
                        self.dispatch_outbound_request(request).await;
                    }
                }
                event = self.swarm.select_next_some() => {
                    if let Some(event) = self.parse_swarm_event(event).await {
                        info!(?event, "Swarm event");
                    }
                }
                enr = async {
                    match &mut self.discovery {
                        Some(disc) => disc.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(enr) = enr {
                        if let Some(multiaddr) = DiscoveryService::enr_to_multiaddr(&enr) {
                            info!(
                                node_id = %enr.node_id(),
                                %multiaddr,
                                "Discovered peer via discv5, attempting connection"
                            );
                            self.connect_to_peers(vec![multiaddr]).await;
                        }
                    }
                }
            }
        }
    }

    async fn parse_swarm_event(
        &mut self,
        event: SwarmEvent<BehaviourEvent>,
    ) -> Option<NetworkEvent> {
        match event {
            SwarmEvent::Behaviour(event) => match event {
                BehaviourEvent::Gossipsub(event) => self.handle_gossipsub_event(event).await,
                BehaviourEvent::Rpc(event) => self.handle_request_response_event(event),
                BehaviourEvent::Identify(event) => self.handle_identify_event(event),
                BehaviourEvent::ConnectionLimits(_) => None,
            },
            SwarmEvent::ConnectionEstablished {
                peer_id, endpoint, ..
            } => {
                self.peer_table
                    .lock()
                    .insert(peer_id, ConnectionState::Connected);

                let connected = self
                    .peer_table
                    .lock()
                    .values()
                    .filter(|s| **s == ConnectionState::Connected)
                    .count() as u64;
                self.peer_count.store(connected, Ordering::Relaxed);

                info!(peer = %peer_id, "Connected to peer (total: {})", connected);

                if endpoint.is_dialer() {
                    self.send_status_request(peer_id);
                }

                METRICS.get().map(|metrics| {
                    metrics.register_peer_connection_success(endpoint.is_listener())
                });

                None
            }
            SwarmEvent::ConnectionClosed {
                peer_id,
                cause,
                endpoint,
                ..
            } => {
                self.peer_table
                    .lock()
                    .insert(peer_id, ConnectionState::Disconnected);

                let connected = self
                    .peer_table
                    .lock()
                    .values()
                    .filter(|s| **s == ConnectionState::Connected)
                    .count() as u64;
                self.peer_count.store(connected, Ordering::Relaxed);

                info!(peer = %peer_id, ?cause, "Disconnected from peer (total: {})", connected);

                METRICS.get().map(|metrics| {
                    let reason = match cause {
                        None => DisconnectReason::LocalClose,
                        Some(ConnectionError::IO(io_error)) => match io_error.kind() {
                            io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset => {
                                DisconnectReason::RemoteClose
                            }
                            io::ErrorKind::TimedOut => DisconnectReason::Timeout,
                            _ => DisconnectReason::Error,
                        },
                        Some(ConnectionError::KeepAliveTimeout) => DisconnectReason::Timeout,
                    };
                    metrics.register_peer_disconnect(endpoint.is_listener(), reason)
                });

                Some(NetworkEvent::PeerDisconnected(peer_id))
            }
            SwarmEvent::IncomingConnection { local_addr, .. } => {
                info!(?local_addr, "Incoming connection");
                None
            }
            SwarmEvent::Dialing { peer_id, .. } => {
                info!(?peer_id, "Dialing peer");
                peer_id.map(NetworkEvent::PeerConnectedOutgoing)
            }
            SwarmEvent::NewListenAddr {
                listener_id,
                address,
            } => {
                info!(?listener_id, ?address, "New listen address");
                None
            }
            SwarmEvent::NewExternalAddrCandidate { address } => {
                info!(?address, "New external address candidate");
                self.swarm.add_external_address(address);
                None
            }
            SwarmEvent::ExternalAddrConfirmed { address } => {
                info!(?address, "External address confirmed");
                None
            }
            SwarmEvent::ExternalAddrExpired { address } => {
                info!(?address, "External address expired");
                None
            }
            SwarmEvent::IncomingConnectionError {
                send_back_addr,
                error,
                ..
            } => {
                warn!(?error, ?send_back_addr, "Incoming connection error");
                METRICS
                    .get()
                    .map(|metrics| metrics.register_peer_connection_failure(true));
                None
            }
            SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                warn!(?peer_id, ?error, "Failed to connect to peer");
                METRICS
                    .get()
                    .map(|metrics| metrics.register_peer_connection_failure(false));
                None
            }
            _ => {
                info!(?event, "Unhandled swarm event");
                None
            }
        }
    }

    async fn handle_gossipsub_event(&mut self, event: GossipsubEvent) -> Option<NetworkEvent> {
        match event {
            GossipsubEvent::Subscribed { peer_id, topic } => {
                info!(peer = %peer_id, topic = %topic, "A peer subscribed to topic");
            }
            GossipsubEvent::Unsubscribed { peer_id, topic } => {
                info!(peer = %peer_id, topic = %topic, "A peer unsubscribed from topic");
            }
            GossipsubEvent::Message { message, .. } => {
                match PubsubMessage::decode(&message.topic, &message.data) {
                    Ok(PubsubMessage::Block(signed_block_with_attestation)) => {
                        let slot = signed_block_with_attestation.message.block.slot.0;
                        if let Err(err) = self
                            .chain_message_sink
                            .send(ChainMessage::ProcessBlock {
                                signed_block_with_attestation,
                                is_trusted: false,
                                should_gossip: true,
                            })
                            .await
                        {
                            warn!(
                                "failed to send block with attestation for slot {slot} to chain: {err:?}"
                            );
                        }
                    }
                    Ok(PubsubMessage::Attestation(signed_attestation)) => {
                        let slot = signed_attestation.message.slot.0;
                        if let Err(err) = self
                            .chain_message_sink
                            .send(ChainMessage::ProcessAttestation {
                                signed_attestation,
                                is_trusted: false,
                                should_gossip: true,
                            })
                            .await
                        {
                            warn!("failed to send vote for slot {slot} to chain: {err:?}");
                        }
                    }
                    Err(err) => {
                        warn!(%err, topic = %message.topic, "gossip decode failed");
                    }
                }
            }
            _ => {
                info!(?event, "Unhandled gossipsub event");
            }
        }
        None
    }

    fn handle_request_response_event(&mut self, event: RpcEvent) -> Option<NetworkEvent> {
        use libp2p::request_response::{Event, Message};

        match event {
            Event::Message { peer, message, .. } => match message {
                Message::Response { response, .. } => match response {
                    LeanResponse::BlocksByRoot(blocks) => {
                        info!(
                            peer = %peer,
                            num_blocks = blocks.len(),
                            "Received BlocksByRoot response"
                        );
                        let chain_sink = self.chain_message_sink.clone();
                        tokio::spawn(async move {
                            for block in blocks {
                                let slot = block.message.block.slot.0;
                                if let Err(e) = chain_sink
                                    .send(ChainMessage::ProcessBlock {
                                        signed_block_with_attestation: block,
                                        is_trusted: false,
                                        should_gossip: false,
                                    })
                                    .await
                                {
                                    warn!(
                                        slot = slot,
                                        ?e,
                                        "Failed to send requested block to chain"
                                    );
                                } else {
                                    debug!(slot = slot, "Queued requested block for processing");
                                }
                            }
                        });
                    }
                    LeanResponse::Status(_) => {
                        info!(peer = %peer, "Received Status response");
                    }
                    LeanResponse::Empty => {
                        warn!(peer = %peer, "Received empty response");
                    }
                },
                Message::Request {
                    request, channel, ..
                } => {
                    let response = match request {
                        LeanRequest::Status(_) => {
                            info!(peer = %peer, "Received Status request");
                            LeanResponse::Status(containers::Status::default())
                        }
                        LeanRequest::BlocksByRoot(roots) => {
                            info!(peer = %peer, num_roots = roots.len(), "Received BlocksByRoot request");
                            LeanResponse::BlocksByRoot(vec![])
                        }
                    };
                    if let Err(e) = self
                        .swarm
                        .behaviour_mut()
                        .rpc
                        .send_response(channel, response)
                    {
                        warn!(peer = %peer, ?e, "Failed to send response");
                    }
                }
            },
            Event::OutboundFailure { peer, error, .. } => {
                warn!(peer = %peer, ?error, "Request failed");
            }
            Event::InboundFailure { peer, error, .. } => {
                warn!(peer = %peer, ?error, "Inbound request failed");
            }
            Event::ResponseSent { peer, .. } => {
                trace!(peer = %peer, "Response sent");
            }
        }
        None
    }

    fn handle_identify_event(&mut self, event: identify::Event) -> Option<NetworkEvent> {
        match event {
            identify::Event::Received { peer_id, info, .. } => {
                info!(
                    peer = %peer_id,
                    agent_version = %info.agent_version,
                    protocol_version = %info.protocol_version,
                    listen_addrs = info.listen_addrs.len(),
                    protocols = info.protocols.len(),
                    "Received peer info"
                );
                None
            }
            identify::Event::Sent { peer_id, .. } => {
                info!(peer = %peer_id, "Sent identify info");
                None
            }
            identify::Event::Pushed { peer_id, .. } => {
                info!(peer = %peer_id, "Pushed identify update");
                None
            }
            identify::Event::Error { peer_id, error, .. } => {
                warn!(peer = %peer_id, ?error, "Identify error");
                None
            }
        }
    }

    async fn connect_to_peers(&mut self, peers: Vec<Multiaddr>) {
        info!(?peers, "Discovered peers");
        for peer in peers {
            if let Some(Protocol::P2p(peer_id)) = peer
                .iter()
                .find(|protocol| matches!(protocol, Protocol::P2p(_)))
                && peer_id != self.local_peer_id()
            {
                let current_state = self.peer_table.lock().get(&peer_id).cloned();
                if !matches!(
                    current_state,
                    Some(ConnectionState::Disconnected | ConnectionState::Connecting) | None
                ) {
                    trace!(?peer_id, "Already connected");
                    continue;
                }

                if let Err(err) = self.swarm.dial(peer.clone()) {
                    warn!(?err, "Failed to dial peer");
                    continue;
                }

                info!(peer = %peer_id, "Dialing peer");
                self.peer_table
                    .lock()
                    .insert(peer_id, ConnectionState::Connecting);
            }
        }
    }

    fn get_random_connected_peer(&self) -> Option<PeerId> {
        let peers: Vec<PeerId> = self
            .peer_table
            .lock()
            .iter()
            .filter(|(_, state)| **state == ConnectionState::Connected)
            .map(|(peer_id, _)| *peer_id)
            .collect();

        if peers.is_empty() {
            None
        } else {
            peers.choose(&mut rand::rng()).copied()
        }
    }

    async fn dispatch_outbound_request(&mut self, request: OutboundP2pRequest) {
        match request {
            OutboundP2pRequest::GossipBlockWithAttestation(signed_block_with_attestation) => {
                let slot = signed_block_with_attestation.message.block.slot.0;
                match signed_block_with_attestation.to_ssz() {
                    Ok(bytes) => {
                        if let Err(err) = self.publish_to_topic(GossipKind::Block, bytes) {
                            warn!(slot = slot, ?err, "Publish block with attestation failed");
                        } else {
                            info!(slot = slot, "Broadcasted block with attestation");
                        }
                    }
                    Err(err) => {
                        warn!(slot = slot, ?err, "Serialize block with attestation failed");
                    }
                }
            }
            OutboundP2pRequest::GossipAttestation(signed_attestation) => {
                let slot = signed_attestation.message.slot.0;
                match signed_attestation.to_ssz() {
                    Ok(bytes) => {
                        if let Err(err) = self.publish_to_topic(GossipKind::Attestation, bytes) {
                            warn!(slot = slot, ?err, "Publish attestation failed");
                        } else {
                            info!(slot = slot, "Broadcasted attestation");
                        }
                    }
                    Err(err) => {
                        warn!(slot = slot, ?err, "Serialize attestation failed");
                    }
                }
            }
            OutboundP2pRequest::RequestBlocksByRoot(roots) => {
                if let Some(peer_id) = self.get_random_connected_peer() {
                    info!(
                        peer = %peer_id,
                        num_blocks = roots.len(),
                        "Requesting missing blocks from peer"
                    );
                    self.send_blocks_by_root_request(peer_id, roots);
                } else {
                    warn!("Cannot request blocks: no connected peers");
                }
            }
        }
    }

    fn publish_to_topic(&mut self, kind: GossipKind, data: Vec<u8>) -> Result<()> {
        let topic = self
            .network_config
            .gossip_topics()
            .into_iter()
            .find(|topic| topic.kind == kind)
            .ok_or_else(|| anyhow!("Missing gossip topic for kind {kind:?}"))?;

        self.swarm
            .behaviour_mut()
            .gossipsub
            .publish(IdentTopic::from(topic), data)
            .map(|_| ())
            .map_err(|err| anyhow!("publish failed: {err:?}"))
    }

    pub fn peer_table(&self) -> Arc<Mutex<HashMap<PeerId, ConnectionState>>> {
        self.peer_table.clone()
    }

    pub fn local_peer_id(&self) -> PeerId {
        *self.swarm.local_peer_id()
    }

    pub fn local_enr(&self) -> Option<&enr::Enr<discv5::enr::CombinedKey>> {
        self.discovery.as_ref().map(|d| d.local_enr())
    }

    pub fn swarm_mut(&mut self) -> &mut Swarm<Behaviour> {
        &mut self.swarm
    }

    fn send_status_request(&mut self, peer_id: PeerId) {
        let status = containers::Status::default();
        let request = LeanRequest::Status(status);
        info!(peer = %peer_id, "Sending Status request for handshake");
        let _request_id = self
            .swarm
            .behaviour_mut()
            .rpc
            .send_request(&peer_id, request);
    }

    pub fn send_blocks_by_root_request(&mut self, peer_id: PeerId, roots: Vec<H256>) {
        if roots.is_empty() {
            return;
        }

        if roots.len() > rpc::methods::MAX_REQUEST_BLOCKS {
            warn!(
                peer = %peer_id,
                requested = roots.len(),
                max = rpc::methods::MAX_REQUEST_BLOCKS,
                "BlocksByRoot request exceeds MAX_REQUEST_BLOCKS"
            );
            return;
        }

        let request = LeanRequest::BlocksByRoot(roots.clone());
        info!(peer = %peer_id, num_roots = roots.len(), "Sending BlocksByRoot request");
        let _request_id = self
            .swarm
            .behaviour_mut()
            .rpc
            .send_request(&peer_id, request);
    }

    fn build_behaviour(local_key: &Keypair, cfg: &NetworkServiceConfig) -> Result<Behaviour> {
        let identify = Self::build_identify(local_key);

        let gossipsub = LeanGossipsub::new_with_transform(
            MessageAuthenticity::Anonymous,
            cfg.gossipsub_config().config.clone(),
            crate::types::pubsub::SnappyTransform::default(),
        )
        .map_err(|err| anyhow!("Failed to create gossipsub behaviour: {err:?}"))?;

        let rpc = rpc::build_rpc();

        let connection_limits = connection_limits::Behaviour::new(
            ConnectionLimits::default()
                .with_max_pending_incoming(Some(5))
                .with_max_pending_outgoing(Some(16))
                .with_max_established_per_peer(Some(2)),
        );

        Ok(Behaviour {
            identify,
            rpc,
            gossipsub,
            connection_limits,
        })
    }

    fn build_identify(local_key: &Keypair) -> identify::Behaviour {
        let local_public_key = local_key.public();
        let identify_config = identify::Config::new("eth2/1.0.0".into(), local_public_key.clone())
            .with_agent_version("0.0.1".to_string())
            .with_cache_size(0);
        identify::Behaviour::new(identify_config)
    }

    fn multiaddr(cfg: &NetworkServiceConfig) -> Result<Multiaddr> {
        let mut addr: Multiaddr = cfg.socket_address.into();
        addr.push(Protocol::Udp(cfg.socket_port));
        addr.push(Protocol::QuicV1);
        Ok(addr)
    }

    fn listen(&mut self, addr: &Multiaddr) -> Result<()> {
        self.swarm
            .listen_on(addr.clone())
            .map_err(|e| anyhow!("Failed to listen on {addr:?}: {e:?}"))?;
        info!(?addr, "Listening on");
        Ok(())
    }

    fn subscribe_to_topics(&mut self) -> Result<()> {
        for topic in self.network_config.gossip_topics() {
            self.swarm
                .behaviour_mut()
                .gossipsub
                .subscribe(&IdentTopic::from(topic.clone()))
                .map_err(|e| anyhow!("Subscribe failed for {topic:?}: {e:?}"))?;
            info!(topic = %topic, "Subscribed to topic");
        }
        Ok(())
    }
}
