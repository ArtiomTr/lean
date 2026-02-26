mod enr_fork_id;
mod globals;
mod pubsub;
mod subnet;
mod sync_state;
mod topics;

use containers::AttestationSubnetCount;
use ssz::BitVector;

pub type EnrAttestationBitfield = BitVector<AttestationSubnetCount>;

pub type Enr = discv5::enr::Enr<discv5::enr::CombinedKey>;

pub use enr_fork_id::EnrForkId;
pub use globals::NetworkGlobals;
pub use pubsub::{PubsubMessage, SnappyTransform};
pub use subnet::{Subnet, SubnetDiscovery};
pub use sync_state::{BackFillState, SyncState};
pub use topics::{
    GossipEncoding, GossipKind, GossipTopic, all_topics_at_fork, core_topics_to_subscribe,
    is_fork_non_core_topic, subnet_from_topic_hash,
};
