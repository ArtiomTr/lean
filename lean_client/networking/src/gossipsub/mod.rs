pub mod message;
pub mod topic;
pub mod config;

use libp2p::gossipsub::{AllowAllSubscriptionFilter, Behaviour};
use crate::compressor::Compressor;

pub type GossipsubBehaviour = Behaviour<Compressor, AllowAllSubscriptionFilter>;