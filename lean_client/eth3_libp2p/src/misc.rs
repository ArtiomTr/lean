use crate::MessageId;

use libp2p::PeerId;
use serde::{Serialize, Serializer};

#[derive(Clone, Debug)]
pub struct GossipId {
    pub source: PeerId,
    pub message_id: MessageId,
}

impl Serialize for GossipId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("GossipId", 2)?;
        state.serialize_field("source", &self.source.to_string())?;
        state.serialize_field("message_id", &hex::encode(self.message_id.0.as_slice()))?;
        state.end()
    }
}

impl Default for GossipId {
    fn default() -> Self {
        Self {
            source: PeerId::from_bytes(&[0; 2]).expect("PeerId byte length should be valid"),
            message_id: MessageId::new(&[]),
        }
    }
}
