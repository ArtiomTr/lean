#![allow(clippy::type_complexity)]
#![allow(clippy::cognitive_complexity)]

use super::methods::{GoodbyeReason, RpcErrorResponse, RpcResponse};
use super::outbound::OutboundRequestContainer;
use super::protocol::{InboundOutput, Protocol, RPCError, RPCProtocol, RequestType};
use super::{RPCReceived, RPCSend, ReqId};
use crate::rpc::outbound::OutboundFramed;
use crate::rpc::protocol::InboundFramed;
use fnv::FnvHashMap;
use futures::SinkExt;
use futures::prelude::*;
use libp2p::PeerId;
use libp2p::swarm::handler::{
    ConnectionEvent, ConnectionHandler, ConnectionHandlerEvent, DialUpgradeError,
    FullyNegotiatedInbound, FullyNegotiatedOutbound, StreamUpgradeError, SubstreamProtocol,
};
use libp2p::swarm::{ConnectionId, Stream};
use logging::exception;
use smallvec::SmallVec;
use std::{
    collections::{VecDeque, hash_map::Entry},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::time::{Sleep, sleep};
use tokio_util::time::{DelayQueue, delay_queue};
use tracing::{debug, trace};

/// The number of times to retry an outbound upgrade in the case of IO errors.
const IO_ERROR_RETRIES: u8 = 3;

/// Maximum time given to the handler to perform shutdown operations.
const SHUTDOWN_TIMEOUT_SECS: u64 = 15;

/// Maximum number of simultaneous inbound substreams we keep for this peer.
const MAX_INBOUND_SUBSTREAMS: usize = 32;

/// Timeout that will be used for inbound and outbound responses.
const RESP_TIMEOUT: Duration = Duration::from_secs(10);

/// Identifier of inbound and outbound substreams from the handler's perspective.
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub struct SubstreamId(usize);

impl SubstreamId {
    pub fn new(id: usize) -> Self {
        Self(id)
    }
}

type InboundSubstream = InboundFramed<Stream>;

/// Events the handler emits to the behaviour.
#[derive(Debug)]
pub enum HandlerEvent<Id> {
    Ok(RPCReceived<Id>),
    Err(HandlerErr<Id>),
    Close(RPCError),
}

/// An error encountered by the handler.
#[derive(Debug)]
pub enum HandlerErr<Id> {
    /// An error occurred for this peer's request.
    Inbound {
        id: SubstreamId,
        proto: Protocol,
        error: RPCError,
    },
    /// An error occurred for this request.
    Outbound {
        id: Id,
        proto: Protocol,
        error: RPCError,
    },
}

/// Implementation of `ConnectionHandler` for the RPC protocol.
pub struct RPCHandler<Id> {
    peer_id: PeerId,
    connection_id: ConnectionId,
    listen_protocol: SubstreamProtocol<RPCProtocol, ()>,
    events_out: SmallVec<[HandlerEvent<Id>; 4]>,
    dial_queue: SmallVec<[(Id, RequestType); 4]>,
    dial_negotiated: u32,
    inbound_substreams: FnvHashMap<SubstreamId, InboundInfo>,
    inbound_substreams_delay: DelayQueue<SubstreamId>,
    outbound_substreams: FnvHashMap<SubstreamId, OutboundInfo<Id>>,
    outbound_substreams_delay: DelayQueue<SubstreamId>,
    current_inbound_substream_id: SubstreamId,
    current_outbound_substream_id: SubstreamId,
    max_dial_negotiated: u32,
    state: HandlerState,
    outbound_io_error_retries: u8,
    waker: Option<std::task::Waker>,
}

enum HandlerState {
    Active,
    ShuttingDown(Pin<Box<Sleep>>),
    Deactivated,
}

struct InboundInfo {
    state: InboundState,
    pending_items: VecDeque<RpcResponse>,
    protocol: Protocol,
    max_remaining_chunks: u64,
    request_start_time: Instant,
    delay_key: Option<delay_queue::Key>,
}

struct OutboundInfo<Id> {
    state: OutboundSubstreamState,
    delay_key: delay_queue::Key,
    proto: Protocol,
    max_remaining_chunks: Option<u64>,
    req_id: Id,
}

enum InboundState {
    Idle(InboundSubstream),
    Busy(Pin<Box<dyn Future<Output = Result<(InboundSubstream, bool), RPCError>> + Send>>),
    Poisoned,
}

pub enum OutboundSubstreamState {
    RequestPendingResponse {
        substream: Box<OutboundFramed<Stream>>,
        request: RequestType,
    },
    Closing(Box<OutboundFramed<Stream>>),
    Poisoned,
}

impl<Id> RPCHandler<Id>
where
    Id: ReqId,
{
    pub fn new(
        listen_protocol: SubstreamProtocol<RPCProtocol, ()>,
        peer_id: PeerId,
        connection_id: ConnectionId,
    ) -> Self {
        RPCHandler {
            peer_id,
            connection_id,
            listen_protocol,
            events_out: SmallVec::new(),
            dial_queue: SmallVec::new(),
            dial_negotiated: 0,
            inbound_substreams: FnvHashMap::default(),
            outbound_substreams: FnvHashMap::default(),
            inbound_substreams_delay: DelayQueue::new(),
            outbound_substreams_delay: DelayQueue::new(),
            current_inbound_substream_id: SubstreamId(0),
            current_outbound_substream_id: SubstreamId(0),
            state: HandlerState::Active,
            max_dial_negotiated: 8,
            outbound_io_error_retries: 0,
            waker: None,
        }
    }

    fn shutdown(&mut self, goodbye_reason: Option<(Id, GoodbyeReason)>) {
        if matches!(self.state, HandlerState::Active) {
            if !self.dial_queue.is_empty() {
                debug!(
                    unsent_queued_requests = self.dial_queue.len(),
                    peer_id = %self.peer_id,
                    connection_id = %self.connection_id,
                    "Starting handler shutdown"
                );
            }
            while let Some((id, req)) = self.dial_queue.pop() {
                self.events_out
                    .push(HandlerEvent::Err(HandlerErr::Outbound {
                        error: RPCError::Disconnected,
                        proto: req.versioned_protocol().protocol(),
                        id,
                    }));
            }

            if let Some((id, reason)) = goodbye_reason {
                self.dial_queue.push((id, RequestType::Goodbye(reason)));
            }

            self.state = HandlerState::ShuttingDown(Box::pin(sleep(Duration::from_secs(
                SHUTDOWN_TIMEOUT_SECS,
            ))));
        }
    }

    fn send_request(&mut self, id: Id, req: RequestType) {
        match self.state {
            HandlerState::Active => {
                self.dial_queue.push((id, req));
            }
            _ => self
                .events_out
                .push(HandlerEvent::Err(HandlerErr::Outbound {
                    error: RPCError::Disconnected,
                    proto: req.versioned_protocol().protocol(),
                    id,
                })),
        }
    }

    fn send_response(&mut self, inbound_id: SubstreamId, response: RpcResponse) {
        let Some(inbound_info) = self.inbound_substreams.get_mut(&inbound_id) else {
            if !matches!(response, RpcResponse::StreamTermination(..)) {
                trace!(%response, id = ?inbound_id,
                    peer_id = %self.peer_id,
                    connection_id = %self.connection_id,
                    "Inbound stream has expired. Response not sent");
            }
            return;
        };

        if let RpcResponse::Error(ref code, ref reason) = response {
            self.events_out.push(HandlerEvent::Err(HandlerErr::Inbound {
                error: RPCError::ErrorResponse(*code, reason.to_string()),
                proto: inbound_info.protocol,
                id: inbound_id,
            }));
        }

        if matches!(self.state, HandlerState::Deactivated) {
            debug!(%response, id = ?inbound_id,
                    peer_id = %self.peer_id,
                    connection_id = %self.connection_id,
                    "Response not sent. Deactivated handler");
            return;
        }

        inbound_info.pending_items.push_back(response);
    }
}

impl<Id> ConnectionHandler for RPCHandler<Id>
where
    Id: ReqId,
{
    type FromBehaviour = RPCSend<Id>;
    type ToBehaviour = HandlerEvent<Id>;
    type InboundProtocol = RPCProtocol;
    type OutboundProtocol = OutboundRequestContainer;
    type OutboundOpenInfo = (Id, RequestType);
    type InboundOpenInfo = ();

    fn listen_protocol(&self) -> SubstreamProtocol<Self::InboundProtocol, ()> {
        self.listen_protocol.clone()
    }

    fn on_behaviour_event(&mut self, rpc_event: Self::FromBehaviour) {
        match rpc_event {
            RPCSend::Request(id, req) => self.send_request(id, req),
            RPCSend::Response(inbound_id, response) => self.send_response(inbound_id, response),
            RPCSend::Shutdown(id, reason) => self.shutdown(Some((id, reason))),
        }
        if let Some(waker) = &self.waker {
            waker.wake_by_ref();
        }
    }

    fn connection_keep_alive(&self) -> bool {
        !matches!(self.state, HandlerState::Deactivated)
    }

    #[allow(deprecated)]
    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<
        ConnectionHandlerEvent<Self::OutboundProtocol, Self::OutboundOpenInfo, Self::ToBehaviour>,
    > {
        if let Some(waker) = &self.waker {
            if !waker.will_wake(cx.waker()) {
                self.waker = Some(cx.waker().clone());
            }
        } else {
            self.waker = Some(cx.waker().clone());
        }

        if !self.events_out.is_empty() {
            return Poll::Ready(ConnectionHandlerEvent::NotifyBehaviour(
                self.events_out.remove(0),
            ));
        } else {
            self.events_out.shrink_to_fit();
        }

        if let HandlerState::ShuttingDown(delay) = &mut self.state {
            match delay.as_mut().poll(cx) {
                Poll::Ready(_) => {
                    self.state = HandlerState::Deactivated;
                    debug!(
                        peer_id = %self.peer_id,
                        connection_id = %self.connection_id,
                        "Shutdown timeout elapsed, Handler deactivated"
                    );
                    return Poll::Ready(ConnectionHandlerEvent::NotifyBehaviour(
                        HandlerEvent::Close(RPCError::Disconnected),
                    ));
                }
                Poll::Pending => {}
            };
        }

        while let Poll::Ready(Some(inbound_id)) = self.inbound_substreams_delay.poll_expired(cx) {
            if let Some(info) = self.inbound_substreams.get_mut(inbound_id.get_ref()) {
                info.delay_key = None;
                self.events_out.push(HandlerEvent::Err(HandlerErr::Inbound {
                    error: RPCError::StreamTimeout,
                    proto: info.protocol,
                    id: *inbound_id.get_ref(),
                }));

                if info.pending_items.back().map(|l| l.close_after()) == Some(false) {
                    info.pending_items.push_back(RpcResponse::Error(
                        RpcErrorResponse::ServerError,
                        "Request timed out".into(),
                    ));
                }
            }
        }

        while let Poll::Ready(Some(outbound_id)) = self.outbound_substreams_delay.poll_expired(cx) {
            if let Some(OutboundInfo { proto, req_id, .. }) =
                self.outbound_substreams.remove(outbound_id.get_ref())
            {
                let outbound_err = HandlerErr::Outbound {
                    id: req_id,
                    proto,
                    error: RPCError::StreamTimeout,
                };
                return Poll::Ready(ConnectionHandlerEvent::NotifyBehaviour(HandlerEvent::Err(
                    outbound_err,
                )));
            } else {
                exception!(peer_id = %self.peer_id,
                    connection_id = %self.connection_id,
                    stream_id = ?outbound_id.get_ref(), "timed out substream not in the books");
            }
        }

        let deactivated = matches!(self.state, HandlerState::Deactivated);

        let mut substreams_to_remove = Vec::new();
        for (id, info) in self.inbound_substreams.iter_mut() {
            loop {
                match std::mem::replace(&mut info.state, InboundState::Poisoned) {
                    InboundState::Idle(substream) if !deactivated => {
                        if let Some(message) = info.pending_items.pop_front() {
                            let last_chunk = info.max_remaining_chunks <= 1;
                            let fut =
                                send_message_to_inbound_substream(substream, message, last_chunk)
                                    .boxed();
                            info.state = InboundState::Busy(Box::pin(fut));
                        } else {
                            info.state = InboundState::Idle(substream);
                            break;
                        }
                    }
                    InboundState::Idle(mut substream) => {
                        match substream.close().poll_unpin(cx) {
                            Poll::Pending => info.state = InboundState::Idle(substream),
                            Poll::Ready(res) => {
                                substreams_to_remove.push(*id);
                                if let Some(ref delay_key) = info.delay_key {
                                    self.inbound_substreams_delay.remove(delay_key);
                                }
                                if let Err(error) = res {
                                    self.events_out.push(HandlerEvent::Err(HandlerErr::Inbound {
                                        error,
                                        proto: info.protocol,
                                        id: *id,
                                    }));
                                }
                                if info.pending_items.back().map(|l| l.close_after()) == Some(false)
                                {
                                    self.events_out.push(HandlerEvent::Err(HandlerErr::Inbound {
                                        error: RPCError::Disconnected,
                                        proto: info.protocol,
                                        id: *id,
                                    }));
                                }
                            }
                        }
                        break;
                    }
                    InboundState::Busy(mut fut) => {
                        match fut.poll_unpin(cx) {
                            Poll::Ready(Ok((substream, substream_was_closed)))
                                if !substream_was_closed =>
                            {
                                info.max_remaining_chunks =
                                    info.max_remaining_chunks.saturating_sub(1);

                                if let Some(ref delay_key) = info.delay_key {
                                    self.inbound_substreams_delay.reset(delay_key, RESP_TIMEOUT);
                                }

                                if !deactivated && !info.pending_items.is_empty() {
                                    if let Some(message) = info.pending_items.pop_front() {
                                        let last_chunk = info.max_remaining_chunks <= 1;
                                        let fut = send_message_to_inbound_substream(
                                            substream, message, last_chunk,
                                        )
                                        .boxed();
                                        info.state = InboundState::Busy(Box::pin(fut));
                                    }
                                } else {
                                    info.state = InboundState::Idle(substream);
                                    break;
                                }
                            }
                            Poll::Ready(Ok((_substream, _substream_was_closed))) => {
                                substreams_to_remove.push(*id);
                                if let Some(ref delay_key) = info.delay_key {
                                    self.inbound_substreams_delay.remove(delay_key);
                                }
                                break;
                            }
                            Poll::Ready(Err(error)) => {
                                substreams_to_remove.push(*id);
                                if let Some(ref delay_key) = info.delay_key {
                                    self.inbound_substreams_delay.remove(delay_key);
                                }
                                self.events_out.push(HandlerEvent::Err(HandlerErr::Inbound {
                                    error,
                                    proto: info.protocol,
                                    id: *id,
                                }));
                                break;
                            }
                            Poll::Pending => {
                                info.state = InboundState::Busy(fut);
                                break;
                            }
                        };
                    }
                    InboundState::Poisoned => unreachable!("Poisoned inbound substream"),
                }
            }
        }

        for inbound_id in substreams_to_remove {
            self.inbound_substreams.remove(&inbound_id);
        }

        for outbound_id in self.outbound_substreams.keys().copied().collect::<Vec<_>>() {
            let (mut entry, state) = match self.outbound_substreams.entry(outbound_id) {
                Entry::Occupied(mut entry) => {
                    let state = std::mem::replace(
                        &mut entry.get_mut().state,
                        OutboundSubstreamState::Poisoned,
                    );
                    (entry, state)
                }
                Entry::Vacant(_) => unreachable!(),
            };

            match state {
                OutboundSubstreamState::RequestPendingResponse {
                    substream,
                    request: _,
                } if deactivated => {
                    entry.get_mut().state = OutboundSubstreamState::Closing(substream);
                    self.events_out
                        .push(HandlerEvent::Err(HandlerErr::Outbound {
                            error: RPCError::Disconnected,
                            proto: entry.get().proto,
                            id: entry.get().req_id,
                        }))
                }
                OutboundSubstreamState::RequestPendingResponse {
                    mut substream,
                    request,
                } => match substream.poll_next_unpin(cx) {
                    Poll::Ready(Some(Ok(response))) => {
                        if request.expect_exactly_one_response() || response.close_after() {
                            entry.get_mut().state = OutboundSubstreamState::Closing(substream);
                        } else {
                            let substream_entry = entry.get_mut();
                            let delay_key = &substream_entry.delay_key;
                            let max_remaining_chunks = substream_entry
                                .max_remaining_chunks
                                .map(|count| count.saturating_sub(1))
                                .unwrap_or_else(|| 0);
                            if max_remaining_chunks == 0 {
                                substream_entry.state = OutboundSubstreamState::Closing(substream);
                            } else {
                                substream_entry.state =
                                    OutboundSubstreamState::RequestPendingResponse {
                                        substream,
                                        request,
                                    };
                                substream_entry.max_remaining_chunks = Some(max_remaining_chunks);
                                self.outbound_substreams_delay
                                    .reset(delay_key, RESP_TIMEOUT);
                            }
                        }

                        let id = entry.get().req_id;
                        let proto = entry.get().proto;

                        let received = match response {
                            RpcResponse::StreamTermination(t) => {
                                HandlerEvent::Ok(RPCReceived::EndOfStream(id, t))
                            }
                            RpcResponse::Success(resp) => {
                                HandlerEvent::Ok(RPCReceived::Response(id, resp))
                            }
                            RpcResponse::Error(ref code, ref r) => {
                                HandlerEvent::Err(HandlerErr::Outbound {
                                    id,
                                    proto,
                                    error: RPCError::ErrorResponse(*code, r.to_string()),
                                })
                            }
                        };

                        return Poll::Ready(ConnectionHandlerEvent::NotifyBehaviour(received));
                    }
                    Poll::Ready(None) => {
                        let delay_key = &entry.get().delay_key;
                        let request_id = entry.get().req_id;
                        self.outbound_substreams_delay.remove(delay_key);
                        entry.remove_entry();
                        if request.expect_exactly_one_response() {
                            return Poll::Ready(ConnectionHandlerEvent::NotifyBehaviour(
                                HandlerEvent::Err(HandlerErr::Outbound {
                                    id: request_id,
                                    proto: request.versioned_protocol().protocol(),
                                    error: RPCError::IncompleteStream,
                                }),
                            ));
                        } else {
                            return Poll::Ready(ConnectionHandlerEvent::NotifyBehaviour(
                                HandlerEvent::Ok(RPCReceived::EndOfStream(
                                    request_id,
                                    request.stream_termination(),
                                )),
                            ));
                        }
                    }
                    Poll::Pending => {
                        entry.get_mut().state =
                            OutboundSubstreamState::RequestPendingResponse { substream, request }
                    }
                    Poll::Ready(Some(Err(e))) => {
                        let delay_key = &entry.get().delay_key;
                        self.outbound_substreams_delay.remove(delay_key);
                        let outbound_err = HandlerErr::Outbound {
                            id: entry.get().req_id,
                            proto: entry.get().proto,
                            error: e,
                        };
                        entry.remove_entry();
                        return Poll::Ready(ConnectionHandlerEvent::NotifyBehaviour(
                            HandlerEvent::Err(outbound_err),
                        ));
                    }
                },
                OutboundSubstreamState::Closing(mut substream) => {
                    match Sink::poll_close(Pin::new(&mut substream), cx) {
                        Poll::Ready(_) => {
                            let delay_key = &entry.get().delay_key;
                            let protocol = entry.get().proto;
                            let request_id = entry.get().req_id;
                            self.outbound_substreams_delay.remove(delay_key);
                            entry.remove_entry();

                            if let Some(termination) = protocol.terminator() {
                                return Poll::Ready(ConnectionHandlerEvent::NotifyBehaviour(
                                    HandlerEvent::Ok(RPCReceived::EndOfStream(
                                        request_id,
                                        termination,
                                    )),
                                ));
                            }
                        }
                        Poll::Pending => {
                            entry.get_mut().state = OutboundSubstreamState::Closing(substream);
                        }
                    }
                }
                OutboundSubstreamState::Poisoned => {
                    exception!(
                        peer_id = %self.peer_id,
                        connection_id = %self.connection_id,
                        "Poisoned outbound substream"
                    );
                    unreachable!("Coding Error: Outbound substream is poisoned")
                }
            }
        }

        if !self.dial_queue.is_empty() && self.dial_negotiated < self.max_dial_negotiated {
            self.dial_negotiated += 1;
            let (id, req) = self.dial_queue.remove(0);
            self.dial_queue.shrink_to_fit();
            return Poll::Ready(ConnectionHandlerEvent::OutboundSubstreamRequest {
                protocol: SubstreamProtocol::new(
                    OutboundRequestContainer {
                        req: req.clone(),
                        max_rpc_size: self.listen_protocol.upgrade().max_rpc_size,
                    },
                    (),
                )
                .map_info(|()| (id, req)),
            });
        }

        if let HandlerState::ShuttingDown(_) = self.state {
            if self.dial_queue.is_empty()
                && self.outbound_substreams.is_empty()
                && self.inbound_substreams.is_empty()
                && self.events_out.is_empty()
                && self.dial_negotiated == 0
            {
                debug!(
                    peer_id = %self.peer_id,
                    connection_id = %self.connection_id,
                    "Goodbye sent, Handler deactivated"
                );

                self.state = HandlerState::Deactivated;
                return Poll::Ready(ConnectionHandlerEvent::NotifyBehaviour(
                    HandlerEvent::Close(RPCError::Disconnected),
                ));
            }
        }

        Poll::Pending
    }

    #[allow(deprecated)]
    fn on_connection_event(
        &mut self,
        event: ConnectionEvent<
            Self::InboundProtocol,
            Self::OutboundProtocol,
            Self::InboundOpenInfo,
            Self::OutboundOpenInfo,
        >,
    ) {
        match event {
            ConnectionEvent::FullyNegotiatedInbound(FullyNegotiatedInbound {
                protocol,
                info: _,
            }) => self.on_fully_negotiated_inbound(protocol),
            ConnectionEvent::FullyNegotiatedOutbound(FullyNegotiatedOutbound {
                protocol,
                info,
            }) => self.on_fully_negotiated_outbound(protocol, info),
            ConnectionEvent::DialUpgradeError(DialUpgradeError { info, error }) => {
                self.on_dial_upgrade_error(info, error)
            }
            _ => {}
        }
    }
}

impl<Id> RPCHandler<Id>
where
    Id: ReqId,
{
    fn on_fully_negotiated_inbound(&mut self, substream: InboundOutput<Stream>) {
        if !matches!(self.state, HandlerState::Active) {
            return;
        }

        let (req, substream) = substream;
        let max_responses = req.max_responses();

        if max_responses > 0 {
            if self.inbound_substreams.len() < MAX_INBOUND_SUBSTREAMS {
                let delay_key = self
                    .inbound_substreams_delay
                    .insert(self.current_inbound_substream_id, RESP_TIMEOUT);
                let awaiting_stream = InboundState::Idle(substream);
                self.inbound_substreams.insert(
                    self.current_inbound_substream_id,
                    InboundInfo {
                        state: awaiting_stream,
                        pending_items: VecDeque::with_capacity(
                            std::cmp::min(max_responses, 128) as usize
                        ),
                        delay_key: Some(delay_key),
                        protocol: req.versioned_protocol().protocol(),
                        request_start_time: Instant::now(),
                        max_remaining_chunks: max_responses,
                    },
                );
            } else {
                self.events_out.push(HandlerEvent::Err(HandlerErr::Inbound {
                    id: self.current_inbound_substream_id,
                    proto: req.versioned_protocol().protocol(),
                    error: RPCError::HandlerRejected,
                }));
                return self.shutdown(None);
            }
        }

        if let RequestType::Goodbye(_) = req {
            self.shutdown(None);
        }

        self.events_out.push(HandlerEvent::Ok(RPCReceived::Request(
            super::InboundRequestId {
                connection_id: self.connection_id,
                substream_id: self.current_inbound_substream_id,
            },
            req,
        )));
        self.current_inbound_substream_id.0 += 1;
    }

    fn on_fully_negotiated_outbound(
        &mut self,
        substream: OutboundFramed<Stream>,
        (id, request): (Id, RequestType),
    ) {
        self.dial_negotiated -= 1;
        self.outbound_io_error_retries = 0;

        let proto = request.versioned_protocol().protocol();

        if matches!(self.state, HandlerState::Deactivated) {
            self.events_out
                .push(HandlerEvent::Err(HandlerErr::Outbound {
                    error: RPCError::Disconnected,
                    proto,
                    id,
                }));
        }

        let max_responses = request.max_responses();
        if max_responses > 0 {
            let max_remaining_chunks = if request.expect_exactly_one_response() {
                None
            } else {
                Some(max_responses)
            };
            let delay_key = self
                .outbound_substreams_delay
                .insert(self.current_outbound_substream_id, RESP_TIMEOUT);
            let awaiting_stream = OutboundSubstreamState::RequestPendingResponse {
                substream: Box::new(substream),
                request,
            };
            if self
                .outbound_substreams
                .insert(
                    self.current_outbound_substream_id,
                    OutboundInfo {
                        state: awaiting_stream,
                        delay_key,
                        proto,
                        max_remaining_chunks,
                        req_id: id,
                    },
                )
                .is_some()
            {
                exception!(
                    peer_id = %self.peer_id,
                    connection_id = %self.connection_id,
                    id = ?self.current_outbound_substream_id, "Duplicate outbound substream id");
            }
            self.current_outbound_substream_id.0 += 1;
        }
    }

    fn on_dial_upgrade_error(
        &mut self,
        request_info: (Id, RequestType),
        error: StreamUpgradeError<RPCError>,
    ) {
        self.dial_negotiated -= 1;

        let (id, req) = request_info;

        let error = match error {
            StreamUpgradeError::Timeout => RPCError::NegotiationTimeout,
            StreamUpgradeError::Apply(RPCError::IoError(e)) => {
                self.outbound_io_error_retries += 1;
                if self.outbound_io_error_retries < IO_ERROR_RETRIES {
                    self.send_request(id, req);
                    return;
                }
                RPCError::IoError(e)
            }
            StreamUpgradeError::NegotiationFailed => RPCError::UnsupportedProtocol,
            StreamUpgradeError::Io(io_err) => {
                self.outbound_io_error_retries += 1;
                if self.outbound_io_error_retries < IO_ERROR_RETRIES {
                    self.send_request(id, req);
                    return;
                }
                RPCError::IoError(io_err.to_string())
            }
            StreamUpgradeError::Apply(other) => other,
        };

        self.outbound_io_error_retries = 0;
        self.events_out
            .push(HandlerEvent::Err(HandlerErr::Outbound {
                error,
                proto: req.versioned_protocol().protocol(),
                id,
            }));
    }
}

async fn send_message_to_inbound_substream(
    mut substream: InboundSubstream,
    message: RpcResponse,
    last_chunk: bool,
) -> Result<(InboundSubstream, bool), RPCError> {
    if matches!(message, RpcResponse::StreamTermination(_)) {
        substream.close().await.map(|_| (substream, true))
    } else {
        let is_error = matches!(message, RpcResponse::Error(..));

        let send_result = substream.send(message).await;

        if last_chunk || is_error || send_result.is_err() {
            let close_result = substream.close().await.map(|_| (substream, true));
            if let Err(e) = send_result {
                return Err(e);
            } else {
                return close_result;
            }
        }
        send_result.map(|_| (substream, false))
    }
}
