//! Event-driven types for the deterministic simulation runtime.
//!
//! The type hierarchy:
//!
//! - **Event**: From EventSources (non-deterministic external world) into Services.
//! - **Effect**: From Services to EventSources (requests to the external world).

use containers::{SignedAttestation, SignedBlockWithAttestation};

use crate::clock::Tick;

/// Events from non-deterministic sources (EventSources).
///
/// Collected by the simulator and dispatched to Services.
#[derive(Debug, Clone)]
pub enum Event {
    /// A clock tick at a specific slot and interval.
    Tick(Tick),
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
