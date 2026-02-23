use super::{
    BehaviourAction, MAX_CONCURRENT_REQUESTS, Protocol, RPCSend, ReqId, RequestType,
    config::OutboundRateLimiterConfig,
    rate_limiter::{RPCRateLimiter as RateLimiter, RateLimitedErr},
};
use crate::rpc::rate_limiter::RateLimiterItem;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{
    collections::{HashMap, VecDeque, hash_map::Entry},
    task::{Context, Poll},
    time::Duration,
};

use futures::FutureExt;
use libp2p::{PeerId, swarm::NotifyHandler};
use logging::exception;
use smallvec::SmallVec;
use tokio_util::time::DelayQueue;
use tracing::debug;

/// A request that was rate limited or waiting on rate limited requests for the same peer and
/// protocol.
struct QueuedRequest<Id: ReqId> {
    req: RequestType,
    request_id: Id,
    queued_at: Duration,
}

/// The number of milliseconds requests delayed due to the concurrent request limit stay in the queue.
const WAIT_TIME_DUE_TO_CONCURRENT_REQUESTS: u64 = 100;

#[allow(clippy::type_complexity)]
pub(crate) struct SelfRateLimiter<Id: ReqId> {
    /// Active requests that are awaiting a response.
    active_requests: HashMap<PeerId, HashMap<Protocol, usize>>,
    /// Requests queued for sending per peer. These requests are stored when the self rate
    /// limiter rejects them. Rate limiting is based on a Peer and Protocol basis, therefore
    /// are stored in the same way.
    delayed_requests: HashMap<(PeerId, Protocol), VecDeque<QueuedRequest<Id>>>,
    /// The delay required to allow a peer's outbound request per protocol.
    next_peer_request: DelayQueue<(PeerId, Protocol)>,
    /// Rate limiter for our own requests.
    rate_limiter: Option<RateLimiter>,
    /// Requests that are ready to be sent.
    ready_requests: SmallVec<[(PeerId, RPCSend<Id>, Duration); 3]>,
}

/// Error returned when the rate limiter does not accept a request.
// NOTE: this is currently not used, but might be useful for debugging.
pub enum Error {
    /// There are queued requests for this same peer and protocol.
    PendingRequests,
    /// Request was tried but rate limited.
    RateLimited,
}

impl<Id: ReqId> SelfRateLimiter<Id> {
    /// Creates a new [`SelfRateLimiter`] based on configuration values.
    pub fn new(config: Option<OutboundRateLimiterConfig>) -> Result<Self, &'static str> {
        debug!(?config, "Using self rate limiting params");
        let rate_limiter = if let Some(c) = config {
            Some(RateLimiter::new_with_config(c.0)?)
        } else {
            None
        };

        Ok(SelfRateLimiter {
            active_requests: Default::default(),
            delayed_requests: Default::default(),
            next_peer_request: Default::default(),
            rate_limiter,
            ready_requests: Default::default(),
        })
    }

    /// Checks if the rate limiter allows the request. If it's allowed, returns the
    /// [`ToSwarm`] that should be emitted. When not allowed, the request is delayed
    /// until it can be sent.
    pub fn allows(
        &mut self,
        peer_id: PeerId,
        request_id: Id,
        req: RequestType,
    ) -> Result<RPCSend<Id>, Error> {
        let protocol = req.versioned_protocol().protocol();
        // First check that there are not already other requests waiting to be sent.
        if let Some(queued_requests) = self.delayed_requests.get_mut(&(peer_id, protocol)) {
            debug!(
                %peer_id,
                protocol = %req.protocol(),
                "Self rate limiting since there are already other requests waiting to be sent"
            );

            queued_requests.push_back(QueuedRequest {
                req,
                request_id,
                queued_at: timestamp_now(),
            });
            return Err(Error::PendingRequests);
        }
        match Self::try_send_request(
            &mut self.active_requests,
            &mut self.rate_limiter,
            peer_id,
            request_id,
            req,
        ) {
            Err((rate_limited_req, wait_time)) => {
                let key = (peer_id, protocol);
                self.next_peer_request.insert(key, wait_time);
                self.delayed_requests
                    .entry(key)
                    .or_default()
                    .push_back(rate_limited_req);

                Err(Error::RateLimited)
            }
            Ok(event) => Ok(event),
        }
    }

    /// Auxiliary function to deal with self rate limiting outcomes. If the rate limiter allows the
    /// request, the [`ToSwarm`] that should be emitted is returned. If the request
    /// should be delayed, it's returned with the duration to wait.
    #[allow(clippy::result_large_err)]
    fn try_send_request(
        active_requests: &mut HashMap<PeerId, HashMap<Protocol, usize>>,
        rate_limiter: &mut Option<RateLimiter>,
        peer_id: PeerId,
        request_id: Id,
        req: RequestType,
    ) -> Result<RPCSend<Id>, (QueuedRequest<Id>, Duration)> {
        if let Some(active_request) = active_requests.get(&peer_id) {
            if let Some(count) = active_request.get(&req.protocol()) {
                if *count >= MAX_CONCURRENT_REQUESTS {
                    debug!(
                        %peer_id,
                        protocol = %req.protocol(),
                        "Self rate limiting due to the number of concurrent requests"
                    );
                    return Err((
                        QueuedRequest {
                            req,
                            request_id,
                            queued_at: timestamp_now(),
                        },
                        Duration::from_millis(WAIT_TIME_DUE_TO_CONCURRENT_REQUESTS),
                    ));
                }
            }
        }

        if let Some(limiter) = rate_limiter.as_mut() {
            match limiter.allows(&peer_id, &req) {
                Ok(()) => {}
                Err(e) => {
                    let protocol = req.versioned_protocol();
                    match e {
                        RateLimitedErr::TooLarge => {
                            // this should never happen with default parameters. Let's just send the request.
                            // Log a exception since this is a config issue.
                            exception!(
                                protocol = %req.versioned_protocol().protocol(),
                                "Self rate limiting error for a batch that will never fit. Sending request anyway. Check configuration parameters.",
                            );
                        }
                        RateLimitedErr::TooSoon(wait_time) => {
                            debug!(
                                protocol = %protocol.protocol(),
                                wait_time_ms = wait_time.as_millis(),
                                %peer_id,
                                "Self rate limiting"
                            );

                            return Err((
                                QueuedRequest {
                                    req,
                                    request_id,
                                    queued_at: timestamp_now(),
                                },
                                wait_time,
                            ));
                        }
                    }
                }
            }
        }

        *active_requests
            .entry(peer_id)
            .or_default()
            .entry(req.protocol())
            .or_default() += 1;

        Ok(RPCSend::Request(request_id, req))
    }

    /// When a peer and protocol are allowed to send a next request, this function checks the
    /// queued requests and attempts marking as ready as many as the limiter allows.
    fn next_peer_request_ready(&mut self, peer_id: PeerId, protocol: Protocol) {
        if let Entry::Occupied(mut entry) = self.delayed_requests.entry((peer_id, protocol)) {
            let queued_requests = entry.get_mut();
            while let Some(QueuedRequest {
                req,
                request_id,
                queued_at,
            }) = queued_requests.pop_front()
            {
                match Self::try_send_request(
                    &mut self.active_requests,
                    &mut self.rate_limiter,
                    peer_id,
                    request_id,
                    req.clone(),
                ) {
                    Err((_rate_limited_req, wait_time)) => {
                        let key = (peer_id, protocol);
                        self.next_peer_request.insert(key, wait_time);
                        // Don't push `rate_limited_req` here to prevent `queued_at` from being updated.
                        queued_requests.push_front(QueuedRequest {
                            req,
                            request_id,
                            queued_at,
                        });
                        // If one fails just wait for the next window that allows sending requests.
                        return;
                    }
                    Ok(event) => self.ready_requests.push((peer_id, event, queued_at)),
                }
            }
            if queued_requests.is_empty() {
                entry.remove();
            }
        }
        // NOTE: There can be entries that have been removed due to peer disconnections, we simply
        // ignore these messages here.
    }

    /// Informs the limiter that a peer has disconnected. This removes any pending requests and
    /// returns their IDs.
    pub fn peer_disconnected(&mut self, peer_id: PeerId) -> Vec<(Id, Protocol)> {
        self.active_requests.remove(&peer_id);

        // It's not ideal to iterate this map, but the key is (PeerId, Protocol) and this map
        // should never really be large. So we iterate for simplicity
        let mut failed_requests = Vec::new();
        self.delayed_requests
            .retain(|(map_peer_id, protocol), queue| {
                if map_peer_id == &peer_id {
                    // NOTE: Currently cannot remove entries from the DelayQueue, we will just let
                    // them expire and ignore them.
                    for message in queue {
                        failed_requests.push((message.request_id, *protocol))
                    }
                    // Remove the entry
                    false
                } else {
                    // Keep the entry
                    true
                }
            });
        failed_requests
    }

    /// Informs the limiter that a response has been received.
    pub fn request_completed(&mut self, peer_id: &PeerId, protocol: Protocol) {
        if let Some(active_requests) = self.active_requests.get_mut(peer_id) {
            if let Entry::Occupied(mut entry) = active_requests.entry(protocol) {
                if *entry.get() > 1 {
                    *entry.get_mut() -= 1;
                } else {
                    entry.remove();
                }
            }
        }
    }

    pub fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<BehaviourAction<Id>> {
        // First check the requests that were self rate limited, since those might add events to
        // the queue. Also do this before rate limiter pruning to avoid removing and
        // immediately adding rate limiting keys.
        if let Poll::Ready(Some(expired)) = self.next_peer_request.poll_expired(cx) {
            let (peer_id, protocol) = expired.into_inner();
            self.next_peer_request_ready(peer_id, protocol);
        }

        // Prune the rate limiter.
        if let Some(limiter) = self.rate_limiter.as_mut() {
            let _ = limiter.poll_unpin(cx);
        }

        // Finally return any queued events.
        if let Some((peer_id, event, _queued_at)) = self.ready_requests.pop() {
            return Poll::Ready(BehaviourAction::NotifyHandler {
                peer_id,
                handler: NotifyHandler::Any,
                event,
            });
        }

        Poll::Pending
    }
}

/// Returns the duration since the unix epoch.
pub fn timestamp_now() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
}
