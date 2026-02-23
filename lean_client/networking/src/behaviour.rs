use libp2p::connection_limits;

pub struct Behaviour {
    /// Keep track of active and pending connections to enforce hard limits.
    pub connection_limits: connection_limits::Behaviour,
    /// The peer manager that keeps track of peer's reputation and status.
    pub peer_manager: PeerManager,
    /// The Eth2 RPC specified in the wire-0 protocol.
    pub eth2_rpc: RPC<RequestId<AppReqId>, P>,
    /// Discv5 Discovery protocol.
    pub discovery: Discovery,
    /// Keep regular connection to peers and disconnect if absent.
    // NOTE: The id protocol is used for initial interop. This will be removed by mainnet.
    /// Provides IP addresses and peer information.
    pub identify: identify::Behaviour,
    /// Libp2p UPnP port mapping.
    pub upnp: Toggle<Upnp>,
    /// The routing pub-sub mechanism for eth2.
    pub gossipsub: Gossipsub,
}
