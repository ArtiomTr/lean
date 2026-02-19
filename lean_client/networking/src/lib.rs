pub mod bootnodes;
pub mod config;
pub mod discovery;
pub mod peer_manager;
pub mod rpc;
pub mod service;
pub mod types;

mod common;
pub mod serde_utils;

// Backward-compatibility re-exports
pub use service::{NetworkEvent, NetworkService, NetworkServiceConfig};
pub use types::{
    ChainMessage, ChainMessageSink, ConnectionState, OutboundP2pRequest, P2pRequestSource,
};

/// Legacy `network` module alias for callers still using `networking::network::*`.
pub mod network {
    pub use crate::service::{NetworkEvent, NetworkService, NetworkServiceConfig};
}

/// Legacy `gossipsub` module alias for callers still using `networking::gossipsub::*`.
pub mod gossipsub {
    pub mod config {
        pub use crate::config::GossipsubConfig;
    }
    pub mod topic {
        pub use crate::types::topics::{
            ATTESTATION_TOPIC, BLOCK_TOPIC, GossipKind as GossipsubKind,
            GossipTopic as GossipsubTopic, SSZ_SNAPPY_ENCODING_POSTFIX, TOPIC_PREFIX, get_topics,
        };
    }
    pub mod message {
        pub use crate::types::pubsub::PubsubMessage as GossipsubMessage;
    }
}
