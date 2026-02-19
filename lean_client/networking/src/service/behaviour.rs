use crate::rpc::Rpc;
use crate::types::pubsub::SnappyTransform;
use libp2p::gossipsub::{AllowAllSubscriptionFilter, Behaviour as GossipsubBehaviour};
use libp2p::{connection_limits, identify, swarm::NetworkBehaviour};

pub type LeanGossipsub = GossipsubBehaviour<SnappyTransform, AllowAllSubscriptionFilter>;

/// Composed NetworkBehaviour for lean ethereum.
///
/// Following eth2_libp2p's Behaviour pattern with the same ordering:
/// connection_limits first to reject connections before other behaviours process them.
#[derive(NetworkBehaviour)]
pub struct Behaviour {
    pub connection_limits: connection_limits::Behaviour,
    pub identify: identify::Behaviour,
    pub rpc: Rpc,
    pub gossipsub: LeanGossipsub,
}
