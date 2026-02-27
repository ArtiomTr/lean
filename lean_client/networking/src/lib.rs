mod behaviour;
mod common;
mod config;
mod defaults;
mod discovery;
mod listen_addr;
mod peer_manager;
mod rpc;
mod service;
mod task_executor;
mod types;

use std::str::FromStr;

use libp2p::swarm::DialError;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

pub use config::Config as NetworkConfig;
pub use discovery::{CombinedKeyExt, EnrExt, Eth2Enr};
pub use libp2p::{Multiaddr, PeerId, gossipsub::TopicHash};
pub use peer_manager::{SyncInfo, peerdb::client::Client};
pub use rpc::{
    BlocksByRootRequest, BlocksByRootRequestV1, InboundRequestId, RequestType, StatusMessage,
    StatusMessageV1,
};
pub use service::{Gossipsub, Network, NetworkEvent, utils::Context as ServiceContext};
pub use task_executor::TaskExecutor;
pub use types::{
    Enr, EnrForkId, GossipTopic, NetworkGlobals, PubsubMessage, Subnet, SubnetDiscovery,
};

/// Wrapper over a libp2p `PeerId` which implements `Serialize` and `Deserialize`
#[derive(Clone, Debug)]
pub struct PeerIdSerialized(libp2p::PeerId);

impl From<PeerIdSerialized> for PeerId {
    fn from(peer_id: PeerIdSerialized) -> Self {
        peer_id.0
    }
}

impl FromStr for PeerIdSerialized {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(
            PeerId::from_str(s).map_err(|e| format!("Invalid peer id: {}", e))?,
        ))
    }
}

impl Serialize for PeerIdSerialized {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for PeerIdSerialized {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: String = Deserialize::deserialize(deserializer)?;
        Ok(Self(PeerId::from_str(&s).map_err(|e| {
            de::Error::custom(format!("Failed to deserialise peer id: {:?}", e))
        })?))
    }
}

// A wrapper struct that prints a dial error nicely.
struct ClearDialError<'a>(&'a DialError);

impl ClearDialError<'_> {
    fn most_inner_error(err: &dyn std::error::Error) -> &dyn std::error::Error {
        let mut current = err;
        while let Some(source) = current.source() {
            current = source;
        }
        current
    }
}

impl std::fmt::Display for ClearDialError<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        match &self.0 {
            DialError::Transport(errors) => {
                for (_, transport_error) in errors {
                    match transport_error {
                        libp2p::TransportError::MultiaddrNotSupported(multiaddr_error) => {
                            write!(f, "Multiaddr not supported: {multiaddr_error}")?;
                        }
                        libp2p::TransportError::Other(other_error) => {
                            let inner_error = ClearDialError::most_inner_error(other_error);
                            write!(f, "Transport error: {inner_error}")?;
                        }
                    }
                }
                Ok(())
            }
            DialError::LocalPeerId { .. } => write!(f, "The peer being dialed is the local peer."),
            DialError::NoAddresses => write!(f, "No addresses for the peer to dial."),
            DialError::DialPeerConditionFalse(_) => write!(f, "PeerCondition evaluation failed."),
            DialError::Aborted => write!(f, "Connection aborted."),
            DialError::WrongPeerId { .. } => write!(f, "Wrong peer id."),
            DialError::Denied { cause } => write!(f, "Connection denied: {:?}", cause),
        }
    }
}
