//! Event-driven types for the deterministic simulation runtime.
//!
//! The type hierarchy:
//!
//! - **Event**: From EventSources (non-deterministic external world) into Services.
//! - **Effect**: From Services to EventSources (requests to the external world).

use anyhow::Result;
use containers::{SignedAttestation, SignedBlockWithAttestation};
use smallvec::SmallVec;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::Span;

use crate::{chain::ChainMessage, clock::Tick, validator::ValidatorMessage};

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

#[derive(Debug, Clone)]
pub enum Message {
    Validator(ValidatorMessage),
    Chain(ChainMessage),
}

impl SpannedEvent {
    pub fn new(span: Span, event: Event) -> Self {
        Self { span, event }
    }
}

/// Effects produced by Services for EventSources to execute.
///
/// Represent side-effects the deterministic core cannot perform itself.
#[derive(Debug, Clone)]
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
pub struct ServiceOutput {
    pub messages: SmallVec<[Message; 1]>,
    pub effects: SmallVec<[Effect; 1]>,
}

impl ServiceOutput {
    #[inline]
    pub fn none() -> Self {
        Self {
            messages: SmallVec::new(),
            effects: SmallVec::new(),
        }
    }

    #[inline]
    pub fn chain_message(msg: ChainMessage) -> Self {
        let mut messages = SmallVec::new();
        messages.push(Message::Chain(msg));
        Self { messages, effects: SmallVec::new() }
    }

    #[inline]
    pub fn validator_message(msg: ValidatorMessage) -> Self {
        let mut messages = SmallVec::new();
        messages.push(Message::Validator(msg));
        Self { messages, effects: SmallVec::new() }
    }

    #[inline]
    pub fn with_chain_message(mut self, msg: ChainMessage) -> Self {
        self.messages.push(Message::Chain(msg));
        self
    }

    #[inline]
    pub fn with_validator_message(mut self, msg: ValidatorMessage) -> Self {
        self.messages.push(Message::Validator(msg));
        self
    }

    #[inline]
    pub fn with_effect(mut self, eff: Effect) -> Self {
        self.effects.push(eff);
        self
    }
}

pub trait Service {
    type Message: 'static + Send;

    fn handle_input(&mut self, input: ServiceInput<Self::Message>) -> ServiceOutput;
}

pub trait EventSource {
    type Event: Send + 'static;
    type Effect;

    fn run(
        &mut self,
        tx: mpsc::UnboundedSender<Self::Event>,
        rx: mpsc::UnboundedReceiver<Self::Effect>,
    ) -> Result<()>;
}
