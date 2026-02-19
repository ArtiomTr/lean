use crate::peer_manager::PeerManager;
use crate::rpc::Rpc;
use crate::types::pubsub::SnappyTransform;
use libp2p::gossipsub::{AllowAllSubscriptionFilter, Behaviour as GossipsubBehaviour};
use libp2p::{connection_limits, identify, swarm::NetworkBehaviour};

/// Composed NetworkBehaviour for lean ethereum.
#[derive(NetworkBehaviour)]
pub struct Behaviour {
    // NOTE: The order of the following list of behaviours has meaning,
    // `NetworkBehaviour::handle_{pending, established}_{inbound, outbound}` methods
    // are called sequentially for each behaviour and they are fallible,
    // therefore we want `connection_limits` and `peer_manager` running first,
    // which are the behaviours that may reject a connection, so that
    // when the subsequent behaviours are called they are certain the connection won't be rejected.

    //
    /// Keep track of active and pending connections to enforce hard limits.
    connection_limits: connection_limits::Behaviour,

    /// The peer manager that keeps track of peer's reputation and status.
    peer_manager: PeerManager,
    /// Provides IP addresses and peer information.
    identify: identify::Behaviour,
    /// Lean RPC protocol
    rpc: Rpc,
    gossipsub: GossipsubBehaviour<SnappyTransform, AllowAllSubscriptionFilter>,
}
