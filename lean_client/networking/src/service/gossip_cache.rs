use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use crate::GossipTopic;
use crate::types::GossipKind;

use tokio_util::time::delay_queue::{DelayQueue, Key};

/// Store of gossip messages that we failed to publish and will try again later. By default, all
/// messages are ignored. This behaviour can be changed using `GossipCacheBuilder::default_timeout`
/// to apply the same delay to every kind. Individual timeouts for specific kinds can be set and
/// will overwrite the default_timeout if present.
pub struct GossipCache {
    /// Expire timeouts for each topic-msg pair.
    expirations: DelayQueue<(GossipTopic, Vec<u8>)>,
    /// Messages cached for each topic.
    topic_msgs: HashMap<GossipTopic, HashMap<Vec<u8>, Key>>,
    /// Timeout for blocks.
    block: Option<Duration>,
    /// Timeout for aggregate attestations.
    aggregates: Option<Duration>,
    /// Timeout for attestations.
    attestation: Option<Duration>,
}

#[derive(Default)]
pub struct GossipCacheBuilder {
    default_timeout: Option<Duration>,
    /// Timeout for blocks.
    block: Option<Duration>,
    /// Timeout for aggregate attestations.
    aggregates: Option<Duration>,
    /// Timeout for attestations.
    attestation: Option<Duration>,
}

#[allow(dead_code)]
impl GossipCacheBuilder {
    /// By default, all timeouts all disabled. Setting a default timeout will enable all timeout
    /// that are not already set.
    pub fn default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = Some(timeout);
        self
    }
    /// Timeout for blocks.
    pub fn block_timeout(mut self, timeout: Duration) -> Self {
        self.block = Some(timeout);
        self
    }

    /// Timeout for aggregate attestations.
    pub fn aggregates_timeout(mut self, timeout: Duration) -> Self {
        self.aggregates = Some(timeout);
        self
    }

    /// Timeout for attestations.
    pub fn attestation_timeout(mut self, timeout: Duration) -> Self {
        self.attestation = Some(timeout);
        self
    }

    pub fn build(self) -> GossipCache {
        let GossipCacheBuilder {
            default_timeout,
            block,
            aggregates,
            attestation,
        } = self;
        GossipCache {
            expirations: DelayQueue::default(),
            topic_msgs: HashMap::default(),
            block: block.or(default_timeout),
            aggregates: aggregates.or(default_timeout),
            attestation: attestation.or(default_timeout),
        }
    }
}

impl GossipCache {
    /// Get a builder of a `GossipCache`. Topic kinds for which no timeout is defined will be
    /// ignored if added in `insert`.
    pub fn builder() -> GossipCacheBuilder {
        GossipCacheBuilder::default()
    }

    // Insert a message to be sent later.
    pub fn insert(&mut self, topic: GossipTopic, data: Vec<u8>) {
        let expire_timeout = match topic.kind() {
            GossipKind::Block => self.block,
            GossipKind::AggregatedAttestation => self.aggregates,
            GossipKind::Attestation(_) => self.attestation,
        };
        let Some(expire_timeout) = expire_timeout else {
            return;
        };
        match self
            .topic_msgs
            .entry(topic.clone())
            .or_default()
            .entry(data.clone())
        {
            Entry::Occupied(key) => self.expirations.reset(key.get(), expire_timeout),
            Entry::Vacant(entry) => {
                let key = self.expirations.insert((topic, data), expire_timeout);
                entry.insert(key);
            }
        }
    }

    // Get the registered messages for this topic.
    pub fn retrieve(&mut self, topic: &GossipTopic) -> Option<impl Iterator<Item = Vec<u8>> + '_> {
        if let Some(msgs) = self.topic_msgs.remove(topic) {
            for (_, key) in msgs.iter() {
                self.expirations.remove(key);
            }
            Some(msgs.into_keys())
        } else {
            None
        }
    }
}

impl futures::stream::Stream for GossipCache {
    type Item = Result<GossipTopic, String>; // We don't care to retrieve the expired data.

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.expirations.poll_expired(cx) {
            Poll::Ready(Some(expired)) => {
                let expected_key = expired.key();
                let (topic, data) = expired.into_inner();
                let topic_msg = self.topic_msgs.get_mut(&topic);
                debug_assert!(
                    topic_msg.is_some(),
                    "Topic for registered message is not present."
                );
                if let Some(msgs) = topic_msg {
                    let key = msgs.remove(&data);
                    debug_assert_eq!(key, Some(expected_key));
                    if msgs.is_empty() {
                        // no more messages for this topic.
                        self.topic_msgs.remove(&topic);
                    }
                }
                Poll::Ready(Some(Ok(topic)))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

// #[cfg(test)]
// mod tests {
//     use super::*;
//     use futures::stream::StreamExt;
//     use types::phase0::primitives::ForkDigest;

//     #[tokio::test]
//     async fn test_stream() {
//         let mut cache = GossipCache::builder()
//             .default_timeout(Duration::from_millis(300))
//             .build();
//         let test_topic = GossipTopic::new(
//             GossipKind::Attestation(1u64.into()),
//             crate::types::GossipEncoding::SSZSnappy,
//             ForkDigest::zero(),
//         );
//         cache.insert(test_topic, vec![]);
//         tokio::time::sleep(Duration::from_millis(300)).await;
//         while cache.next().await.is_some() {}
//         assert!(cache.expirations.is_empty());
//         assert!(cache.topic_msgs.is_empty());
//     }
// }
