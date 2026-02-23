use containers::{SignedAttestation, SignedBlockWithAttestation};
use ssz::H256;

/// Messages sent from the network layer to chain services.
#[derive(Debug, Clone)]
pub enum ChainMessage {
    /// A new block received from the network.
    ProcessBlock {
        signed_block_with_attestation: SignedBlockWithAttestation,
    },
    /// A new attestation received from the network.
    ProcessAttestation {
        signed_attestation: SignedAttestation,
    },
}

/// Requests sent from the application to the network layer.
#[derive(Debug, Clone)]
pub enum OutboundP2pRequest {
    /// Gossip a signed block with attestation to all peers.
    GossipBlockWithAttestation(SignedBlockWithAttestation),
    /// Gossip a signed attestation to all peers.
    GossipAttestation(SignedAttestation),
    /// Request blocks by root hash from peers.
    RequestBlocksByRoot(Vec<H256>),
}
