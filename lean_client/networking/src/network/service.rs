use std::{
    collections::HashMap,
    convert::TryInto,
    fs,
    net::IpAddr,
    num::{NonZeroU8, NonZeroUsize},
    path::PathBuf,
    sync::Arc,
};

use alloy_primitives::hex;
use anyhow::{Result, anyhow};
use containers::ssz::SszWrite;
use futures::StreamExt;
use libp2p::{
    Multiaddr, SwarmBuilder,
    connection_limits::{self, ConnectionLimits},
    gossipsub::{Event as GossipsubEvent, IdentTopic, MessageAuthenticity},
    identify,
    multiaddr::Protocol,
    swarm::{Config, Swarm, SwarmEvent},
};
use libp2p_identity::{Keypair, PeerId, secp256k1};
use parking_lot::Mutex;
use tokio::select;
use tracing::{info, trace, warn};

use crate::{
    bootnodes::BootnodeSource,
    compressor::Compressor,
    executor::{Executor, TaskExecutor},
    gossipsub::{self, config::GossipsubConfig, message::GossipsubMessage, topic::GossipsubKind},
    network::behaviour::{LeanNetworkBehaviour, LeanNetworkBehaviourEvent},
    req_resp::{self, ReqRespMessage},
    types::{
        ChainMessage, ChainMessageSink, ConnectionState, OutboundP2pRequest, P2pRequestSource,
    },
};

#[derive(Debug, Clone)]
pub struct LeanNetworkConfig {
    pub gossipsub_config: GossipsubConfig,
    pub socket_address: IpAddr,
    pub socket_port: u16,
    pub private_key_path: Option<PathBuf>,
    pub request_response_protocols: Vec<String>,
}

impl LeanNetworkConfig {
    pub fn new(
        gossipsub_config: GossipsubConfig,
        socket_address: IpAddr,
        socket_port: u16,
    ) -> Self {
        LeanNetworkConfig {
            gossipsub_config,
            socket_address,
            socket_port,
            private_key_path: None,
            request_response_protocols: Vec::new(),
        }
    }

    pub fn add_request_response_protocol<S: Into<String>>(&mut self, protocol: S) {
        self.request_response_protocols.push(protocol.into());
    }
}

#[derive(Debug)]
pub enum LeanNetworkEvent {
    PeerConnectedIncoming(PeerId),
    PeerConnectedOutgoing(PeerId),
    PeerDisconnected(PeerId),
    Status(PeerId),
    Ping(PeerId),
    MetaData(PeerId),
    DisconnectPeer(PeerId),
}

pub struct LeanNetworkService<C, R>
where
    C: ChainMessageSink<ChainMessage> + Send + Sync + 'static,
    R: P2pRequestSource<OutboundP2pRequest> + Send + 'static,
{
    network_config: Arc<LeanNetworkConfig>,
    swarm: Swarm<LeanNetworkBehaviour>,
    peer_table: Arc<Mutex<HashMap<PeerId, ConnectionState>>>,
    chain_message_sink: C,
    outbound_p2p_requests: R,
}

impl<C, R> LeanNetworkService<C, R>
where
    C: ChainMessageSink<ChainMessage> + Send + Sync + 'static,
    R: P2pRequestSource<OutboundP2pRequest> + Send + 'static,
{
    pub async fn new<E: TaskExecutor>(
        network_config: Arc<LeanNetworkConfig>,
        executor: E,
        chain_message_sink: C,
        outbound_p2p_requests: R,
    ) -> Result<Self> {
        let connection_limits = {
            let limits = ConnectionLimits::default()
                .with_max_pending_incoming(Some(5))
                .with_max_pending_outgoing(Some(16))
                .with_max_established_per_peer(Some(2));

            connection_limits::Behaviour::new(limits)
        };

        let local_key = load_or_generate_local_key(network_config.private_key_path.as_ref())?;

        let gossipsub = gossipsub::GossipsubBehaviour::new_with_transform(
            MessageAuthenticity::Anonymous,
            network_config.gossipsub_config.config.clone(),
            Compressor::default(),
        )
        .map_err(|err| anyhow!("failed to create gossipsub behaviour: {err:?}"))?;

        let identify = build_identify(&local_key);

        let request_protocols = if network_config.request_response_protocols.is_empty() {
            vec!["/lean/req/1".to_string()]
        } else {
            network_config.request_response_protocols.clone()
        };

        let req_resp = req_resp::build(request_protocols);

        let behaviour = LeanNetworkBehaviour {
            identify,
            req_resp,
            gossipsub,
            connection_limits,
        };

        let config = Config::with_executor(Executor(executor))
            .with_notify_handler_buffer_size(NonZeroUsize::new(7).expect("non-zero"))
            .with_per_connection_event_buffer_size(4)
            .with_dial_concurrency_factor(NonZeroU8::new(1).expect("non-zero"));

        let mut multi_addr: Multiaddr = network_config.socket_address.into();
        multi_addr.push(Protocol::Udp(network_config.socket_port));
        multi_addr.push(Protocol::QuicV1);

        let swarm = SwarmBuilder::with_existing_identity(local_key.clone())
            .with_tokio()
            .with_quic()
            .with_behaviour(|_| behaviour)?
            .with_swarm_config(|_| config)
            .build();

        let mut service = LeanNetworkService {
            network_config,
            swarm,
            peer_table: Arc::new(Mutex::new(HashMap::new())),
            chain_message_sink,
            outbound_p2p_requests,
        };

        service
            .swarm
            .listen_on(multi_addr.clone())
            .map_err(|err| anyhow!("failed to start libp2p listen on {multi_addr:?}: {err:?}"))?;

        info!(?multi_addr, "Listening on");

        for topic in &service.network_config.gossipsub_config.topics {
            service
                .swarm
                .behaviour_mut()
                .gossipsub
                .subscribe(&IdentTopic::from(topic.clone()))
                .map_err(|err| anyhow!("subscribe to {topic:?} failed: {err:?}"))?;
        }

        Ok(service)
    }

    pub async fn start<B>(&mut self, bootnodes: B) -> Result<()>
    where
        B: BootnodeSource,
    {
        info!("LeanNetworkService started");

        self.connect_to_peers(bootnodes.to_multiaddrs()).await;

        loop {
            select! {
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
            }
        }
    }

    async fn parse_swarm_event(
        &mut self,
        event: SwarmEvent<LeanNetworkBehaviourEvent>,
    ) -> Option<LeanNetworkEvent> {
        match event {
            SwarmEvent::Behaviour(LeanNetworkBehaviourEvent::Gossipsub(event)) => {
                self.handle_gossipsub_event(event).await
            }
            SwarmEvent::Behaviour(LeanNetworkBehaviourEvent::ReqResp(event)) => {
                self.handle_request_response_event(event)
            }
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                self.peer_table
                    .lock()
                    .insert(peer_id, ConnectionState::Connected);

                info!(peer = %peer_id, "Connected to peer");
                None
            }
            SwarmEvent::ConnectionClosed { peer_id, .. } => {
                self.peer_table
                    .lock()
                    .insert(peer_id, ConnectionState::Disconnected);

                info!(peer = %peer_id, "Disconnected from peer");
                Some(LeanNetworkEvent::PeerDisconnected(peer_id))
            }
            SwarmEvent::IncomingConnection { local_addr, .. } => {
                info!(?local_addr, "Incoming connection");
                None
            }
            SwarmEvent::Dialing { peer_id, .. } => {
                info!(?peer_id, "Dialing peer");
                peer_id.map(LeanNetworkEvent::PeerConnectedOutgoing)
            }
            SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                warn!(?peer_id, ?error, "Failed to connect to peer");
                None
            }
            _ => None,
        }
    }

    async fn handle_gossipsub_event(&mut self, event: GossipsubEvent) -> Option<LeanNetworkEvent> {
        if let GossipsubEvent::Message { message, .. } = event {
            match GossipsubMessage::decode(&message.topic, &message.data) {
                Ok(GossipsubMessage::Block(signed_block)) => {
                    let slot = signed_block.message.slot.0;

                    if let Err(err) = self
                        .chain_message_sink
                        .send(ChainMessage::block(signed_block))
                        .await
                    {
                        warn!(slot = slot, error = %err, "failed to forward block to chain");
                    }
                }
                Ok(GossipsubMessage::Vote(signed_vote)) => {
                    let slot = signed_vote.data.slot.0;

                    if let Err(err) = self
                        .chain_message_sink
                        .send(ChainMessage::vote(signed_vote))
                        .await
                    {
                        warn!(slot = slot, error = %err, "failed to forward vote to chain");
                    }
                }
                Err(err) => warn!(%err, "gossip decode failed"),
            }
        }
        None
    }

    fn handle_request_response_event(
        &mut self,
        _event: ReqRespMessage,
    ) -> Option<LeanNetworkEvent> {
        None
    }

    async fn connect_to_peers(&mut self, peers: Vec<Multiaddr>) {
        trace!(?peers, "Discovered peers");
        for peer in peers {
            if let Some(Protocol::P2p(peer_id)) = peer
                .iter()
                .find(|protocol| matches!(protocol, Protocol::P2p(_)))
                && peer_id != self.local_peer_id()
            {
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

    async fn dispatch_outbound_request(&mut self, request: OutboundP2pRequest) {
        match request {
            OutboundP2pRequest::GossipBlock(signed_block) => {
                let slot = signed_block.message.slot.0;
                match signed_block.to_ssz() {
                    Ok(bytes) => {
                        if let Err(err) = self.publish_to_topic(GossipsubKind::Block, bytes) {
                            warn!(slot = slot, ?err, "Publish block failed");
                        } else {
                            info!(slot = slot, "Broadcasted block");
                        }
                    }
                    Err(err) => {
                        warn!(slot = slot, ?err, "Serialize block failed");
                    }
                }
            }
            OutboundP2pRequest::GossipVote(signed_vote) => {
                let slot = signed_vote.data.slot.0;
                match signed_vote.to_ssz() {
                    Ok(bytes) => {
                        if let Err(err) = self.publish_to_topic(GossipsubKind::Vote, bytes) {
                            warn!(slot = slot, ?err, "Publish vote failed");
                        } else {
                            info!(slot = slot, "Broadcasted vote");
                        }
                    }
                    Err(err) => {
                        warn!(slot = slot, ?err, "Serialize vote failed");
                    }
                }
            }
        }
    }

    fn publish_to_topic(&mut self, kind: GossipsubKind, data: Vec<u8>) -> Result<()> {
        let topic = self
            .network_config
            .gossipsub_config
            .topics
            .iter()
            .find(|topic| topic.kind == kind)
            .cloned()
            .ok_or_else(|| anyhow!("Missing gossipsub topic for kind {kind:?}"))?;

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

    pub fn swarm_mut(&mut self) -> &mut Swarm<LeanNetworkBehaviour> {
        &mut self.swarm
    }
}

fn load_or_generate_local_key(path: Option<&PathBuf>) -> Result<Keypair> {
    if let Some(path) = path {
        let private_key_hex = fs::read_to_string(path)
            .map_err(|err| anyhow!("failed to read secret key file {}: {err}", path.display()))?;
        let private_key_bytes = hex::decode(private_key_hex.trim()).map_err(|err| {
            anyhow!(
                "failed to decode hex from private key file {}: {err}",
                path.display()
            )
        })?;
        let private_key_array: [u8; 32] = private_key_bytes
            .try_into()
            .map_err(|_| anyhow!("invalid secret key length"))?;
        let private_key = secp256k1::SecretKey::try_from_bytes(private_key_array)
            .map_err(|err| anyhow!("failed to decode secp256k1 secret key from bytes: {err}"))?;

        Ok(Keypair::from(secp256k1::Keypair::from(private_key)))
    } else {
        Ok(Keypair::generate_secp256k1())
    }
}

fn build_identify(local_key: &Keypair) -> identify::Behaviour {
    let local_public_key = local_key.public();
    let identify_config = identify::Config::new("eth2/1.0.0".into(), local_public_key.clone())
        .with_agent_version("0.0.1".to_string())
        .with_cache_size(0);

    identify::Behaviour::new(identify_config)
}
