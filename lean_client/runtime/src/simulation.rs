//! Event-driven types for the deterministic simulation runtime.
//!
//! The type hierarchy:
//!
//! - **Event**: From EventSources (non-deterministic external world) into Services.
//! - **Effect**: From Services to EventSources (requests to the external world).

use containers::{SignedAttestation, SignedBlockWithAttestation};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::Span;

use crate::clock::Tick;

/// Events from non-deterministic sources (EventSources).
///
/// Collected by the simulator and dispatched to Services.
#[derive(Debug, Clone)]
pub enum Event {
    /// A clock tick at a specific slot and interval.
    Tick(Tick),
}

#[derive(Debug, Clone)]
pub struct SpannedEvent {
    span: Span,
    event: Event,
}

impl SpannedEvent {
    pub fn new(span: Span, event: Event) -> Self {
        Self { span, event }
    }
}

/// Effects produced by Services for EventSources to execute.
///
/// Represent side-effects the deterministic core cannot perform itself.
pub enum Effect {
    /// Gossip a signed block with proposer attestation to the network.
    GossipBlock(SignedBlockWithAttestation),

    /// Gossip a signed attestation to the network.
    GossipAttestation(SignedAttestation),
}

#[derive(Debug, Clone)]
pub enum ServiceInput<T> {
    Event(Event),
    Message(T),
}

#[derive(Debug, Clone)]
pub struct ServiceOutput {}

pub trait Service {
    type Message;

    fn handle_input(&mut self, input: ServiceInput<Self::Message>) -> ServiceOutput;
}

pub trait EventSource {
    type Event: Send + 'static;
    type Effect;

    fn run(
        &mut self,
        tx: mpsc::UnboundedSender<Self::Event>,
        rx: mpsc::UnboundedReceiver<Self::Effect>,
    );
}
