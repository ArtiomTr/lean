//! Chain service that drives the consensus clock and owns the fork choice store.
//!
//! Every 4 seconds (1 slot), the forkchoice store processes 5 intervals:
//!
//! - Interval 0: Block proposal window
//! - Interval 1: Attestation broadcast window
//! - Interval 2: Aggregation window
//! - Interval 3: Safe target update
//! - Interval 4: Accept new attestations into fork choice
//!
//! The `ChainService` is the heartbeat. It receives tick events, advances the
//! store, and notifies other services of the current state via Messages.
//!
//! It also handles requests from other services (block production, attestation
//! processing) since it is the sole owner of the Store.
//!
//! ## Network events
//!
//! When blocks or attestations arrive from the P2P network (`Event::Network`),
//! `ChainService` inserts them into the fork choice store. If a block's parent
//! is not yet in the store, it emits `NetworkMessage::RequestBlocksByRoot` so
//! `NetworkService` can fetch the missing ancestor from peers.

use anyhow::Result;
use clock::{Interval, Tick};
use containers::{
    AttestationData, Checkpoint, SignedAggregatedAttestation, SignedAttestation,
    SignedBlockWithAttestation, Slot,
};
use fork_choice::Store;
use networking::{BlocksByRootRequest, InboundRequestId, PeerId, StatusMessage, StatusMessageV1};
use ssz::SszHash as _;
use std::{collections::HashMap, sync::Arc};
use tracing::{debug, info, warn};

use crate::{
    environment::{Event, NetworkEvent, Service, ServiceInput, ServiceOutput},
    network::NetworkMessage,
    validator::ValidatorMessage,
};

/// Messages that ChainService receives (input to the state machine).
#[derive(Debug, Clone)]
pub enum ChainMessage {
    /// ValidatorService → ChainService: pull slot state to decide duties.
    GetSlotData {
        slot: Slot,
        interval: Interval,
    },

    /// ValidatorService → ChainService: request block production.
    ProduceBlock {
        slot: Slot,
        proposer_idx: u64,
    },

    /// ValidatorService → ChainService: process attestation in the store.
    ProcessAttestation(SignedAttestation),

    ProcessNetworkBlock(Arc<SignedBlockWithAttestation>),
    ProcessNetworkAttestation(Arc<SignedAttestation>),
    ProcessNetworkAggregatedAttestation(Arc<SignedAggregatedAttestation>),
    HandleStatusRequest {
        peer_id: PeerId,
        inbound_request_id: InboundRequestId,
        request: StatusMessage,
    },
    SendStatusRequest {
        peer_id: PeerId,
    },
    HandleBlocksByRootsRequest {
        peer_id: PeerId,
        inbound_request_id: InboundRequestId,
        request: BlocksByRootRequest,
    },
}

/// Drives the consensus clock and owns the forkchoice store.
///
/// All store mutations happen here. Other services request state changes
/// through Messages.
#[derive(Clone)]
pub struct ChainService {
    store: Store,
    signed_blocks: HashMap<ssz::H256, Arc<SignedBlockWithAttestation>>,
}

impl ChainService {
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self {
            store,
            signed_blocks: HashMap::new(),
        }
    }

    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Construct `AttestationData` from the current store state.
    ///
    /// Per spec: head checkpoint from current head, target from
    /// `get_vote_target`, source from `latest_justified`.
    fn attestation_data(&self, slot: Slot) -> Result<AttestationData> {
        self.store.produce_attestation_data(slot)
    }

    /// Process a block from the network and return any required effects.
    ///
    /// On success, the block is inserted into the fork choice store.
    /// When the parent block is unknown, emits `NetworkMessage::RequestBlocksByRoot` so the
    /// missing ancestor can be fetched from peers.
    fn process_network_block(&mut self, signed_block: SignedBlockWithAttestation) -> ServiceOutput {
        let block_root = signed_block.message.block.hash_tree_root();
        let slot = signed_block.message.block.slot.0;
        let parent_root = signed_block.message.block.parent_root;

        self.signed_blocks
            .insert(block_root, Arc::new(signed_block.clone()));

        match self.store.on_block(signed_block) {
            Ok(()) => {
                info!(slot, "Network block processed");
                ServiceOutput::none()
            }
            Err(err) => {
                let err_str = format!("{err:?}");
                if err_str.contains("Block queued") {
                    // Parent not yet in the store — ask the network for it.
                    debug!(
                        slot,
                        parent_root = %format_args!("0x{parent_root:x}"),
                        "Block queued awaiting parent; requesting from peers",
                    );

                    if parent_root.is_zero() {
                        // Genesis parent — no-one to ask.
                        return ServiceOutput::none();
                    }

                    ServiceOutput::network_message(NetworkMessage::RequestBlocksByRoot(vec![
                        parent_root,
                    ]))
                } else {
                    warn!(%err, slot, "Failed to process network block");
                    ServiceOutput::none()
                }
            }
        }
    }

    /// Process an attestation from the network.
    fn process_network_attestation(&mut self, attestation: SignedAttestation) -> ServiceOutput {
        let validator_id = attestation.validator_id;
        if let Err(err) = self.store.on_gossip_attestation(&attestation, false) {
            warn!(%err, validator = validator_id, "Failed to process network attestation");
        }
        ServiceOutput::none()
    }

    fn process_network_aggregated_attestation(
        &mut self,
        attestation: SignedAggregatedAttestation,
    ) -> ServiceOutput {
        let participants = attestation.proof.get_participant_indices().len();
        if let Err(err) = self.store.on_gossip_aggregated_attestation(&attestation) {
            warn!(%err, participants, "Failed to process network aggregated attestation");
        }
        ServiceOutput::none()
    }
}

impl Service for ChainService {
    type Message = ChainMessage;

    fn handle_input(&mut self, input: ServiceInput<Self::Message>) -> ServiceOutput {
        match input {
            // ── Tick flow ────────────────────────────────────────────────────
            //
            // Every interval: advance the store clock only.
            // ValidatorService drives its own duties by sending GetSlotData.
            ServiceInput::Event(Event::Tick(Tick { slot, interval })) => {
                // let genesis_time = self.store.config.genesis_time;
                // let interval_index = u64::from(interval as u8);
                // let current_time =
                //     genesis_time + slot * SECONDS_PER_SLOT + interval_index * SECONDS_PER_INTERVAL;

                // on_tick(&mut self.store, current_time, false);

                // debug!(slot, interval = ?interval, store_time = self.store.time, "Chain tick processed");

                // ServiceOutput::none()

                if let Err(err) = self.store.on_tick(Slot(slot), interval, false, false) {
                    warn!(%err, slot, interval = ?interval, "Failed to advance forkchoice tick");
                    return ServiceOutput::none();
                }

                ServiceOutput::none()
            }

            // ── Network block flow ───────────────────────────────────────────
            //
            // A peer gossiped a block. Insert it into fork choice. If the parent
            // is unknown, request it from the network.
            ServiceInput::Event(Event::Network(NetworkEvent::GossipBlock(signed_block))) => {
                self.process_network_block(signed_block.as_ref().clone())
            }

            // ── Network attestation flow ─────────────────────────────────────
            //
            // A peer gossiped an attestation. Feed it into fork choice.
            ServiceInput::Event(Event::Network(NetworkEvent::GossipAttestation(attestation))) => {
                self.process_network_attestation(attestation.as_ref().clone())
            }

            // ── Network aggregated attestation flow ───────────────────────────
            //
            // A peer gossiped an aggregated attestation.
            ServiceInput::Event(Event::Network(NetworkEvent::GossipAggregatedAttestation(
                attestation,
            ))) => self.process_network_aggregated_attestation(attestation.as_ref().clone()),

            // ── RPC status request flow ────────────────────────────────────────
            //
            // Received a status RPC request.
            ServiceInput::Event(Event::Network(NetworkEvent::RpcStatusRequest {
                peer_id,
                inbound_request_id,
                request,
            })) => ServiceOutput::chain_message(ChainMessage::HandleStatusRequest {
                peer_id,
                inbound_request_id,
                request,
            }),

            // ── RPC blocks by roots request flow ───────────────────────────────
            //
            // Received a blocks by roots RPC request.
            ServiceInput::Event(Event::Network(NetworkEvent::RpcBlocksByRootsRequest {
                peer_id,
                inbound_request_id,
                request,
            })) => ServiceOutput::chain_message(ChainMessage::HandleBlocksByRootsRequest {
                peer_id,
                inbound_request_id,
                request,
            }),

            ServiceInput::Event(Event::Network(_)) => ServiceOutput::none(),

            // ── GetSlotData flow ─────────────────────────────────────────────
            //
            // ValidatorService pulls slot state to decide its duties.
            // Called after the tick for the same slot/interval has been processed,
            // so on_tick has already run and the store is up to date.
            ServiceInput::Message(ChainMessage::GetSlotData { slot, interval }) => {
                let num_validators = self
                    .store
                    .states()
                    .get(&self.store.head())
                    .map(|s| s.validators.len_u64())
                    .unwrap_or(0);

                let attestation_data = match self.attestation_data(slot) {
                    Ok(data) => data,
                    Err(err) => {
                        warn!(%err, slot = slot.0, interval = ?interval, "Failed to build attestation data");
                        return ServiceOutput::none();
                    }
                };

                ServiceOutput::validator_message(ValidatorMessage::SlotData {
                    slot,
                    interval,
                    num_validators,
                    attestation_data,
                })
            }

            // ── Block production flow ────────────────────────────────────────
            //
            // ValidatorService determined it's the proposer and asks us to
            // build a block. We insert it into the store, then hand it back
            // with fresh attestation data so the proposer can sign and gossip.
            ServiceInput::Message(ChainMessage::ProduceBlock { slot, proposer_idx }) => {
                match self.store.produce_block_with_signatures(slot, proposer_idx) {
                    Ok((block_root, block, signatures)) => {
                        info!(
                            slot = slot.0,
                            block_root = %format_args!("0x{block_root:x}"),
                            proposer = proposer_idx,
                            "Block produced and stored",
                        );

                        // Recompute attestation data after block insertion so the
                        // proposer's attestation reflects the updated chain head.
                        ServiceOutput::validator_message(ValidatorMessage::BlockProduced {
                            block,
                            block_root,
                            signatures,
                            attestation_data: match self.attestation_data(slot) {
                                Ok(data) => data,
                                Err(err) => {
                                    warn!(%err, slot = slot.0, "Failed to build attestation data for produced block");
                                    return ServiceOutput::none();
                                }
                            },
                        })
                    }
                    Err(err) => {
                        warn!(%err, slot = slot.0, proposer = proposer_idx, "Failed to produce block");
                        ServiceOutput::none()
                    }
                }
            }

            // ── Attestation flow ─────────────────────────────────────────────
            //
            // ValidatorService produced an attestation; feed it into fork
            // choice. No response needed — fork choice updates silently.
            ServiceInput::Message(ChainMessage::ProcessAttestation(att)) => {
                let validator_id = att.validator_id;
                if let Err(err) = self.store.on_gossip_attestation(&att, false) {
                    warn!(%err, validator = validator_id, "Failed to process attestation in store");
                }
                ServiceOutput::none()
            }

            ServiceInput::Message(ChainMessage::ProcessNetworkBlock(signed_block)) => {
                self.process_network_block(signed_block.as_ref().clone())
            }

            ServiceInput::Message(ChainMessage::ProcessNetworkAttestation(attestation)) => {
                self.process_network_attestation(attestation.as_ref().clone())
            }

            ServiceInput::Message(ChainMessage::ProcessNetworkAggregatedAttestation(
                attestation,
            )) => self.process_network_aggregated_attestation(attestation.as_ref().clone()),

            ServiceInput::Message(ChainMessage::HandleStatusRequest {
                peer_id,
                inbound_request_id,
                request: _,
            }) => {
                let head_root = self.store.head();
                let head_slot = self
                    .store
                    .blocks()
                    .get(&head_root)
                    .map(|block| block.slot)
                    .unwrap_or(Slot(0));

                let response = StatusMessage::V1(StatusMessageV1 {
                    finalized: *self.store.latest_finalized(),
                    head: Checkpoint {
                        root: head_root,
                        slot: head_slot,
                    },
                });

                ServiceOutput::network_message(NetworkMessage::SendStatusResponse {
                    peer_id,
                    inbound_request_id,
                    status: response,
                })
            }

            ServiceInput::Message(ChainMessage::SendStatusRequest { peer_id }) => {
                let head_root = self.store.head();
                let head_slot = self
                    .store
                    .blocks()
                    .get(&head_root)
                    .map(|block| block.slot)
                    .unwrap_or(Slot(0));

                let status = StatusMessage::V1(StatusMessageV1 {
                    finalized: *self.store.latest_finalized(),
                    head: Checkpoint {
                        root: head_root,
                        slot: head_slot,
                    },
                });

                ServiceOutput::network_message(NetworkMessage::SendStatusRequest {
                    peer_id,
                    status,
                })
            }

            ServiceInput::Message(ChainMessage::HandleBlocksByRootsRequest {
                peer_id,
                inbound_request_id,
                request,
            }) => {
                let mut output = ServiceOutput::none();

                for root in request.block_roots() {
                    if let Some(block) = self.signed_blocks.get(&root) {
                        output =
                            output.with_network_message(NetworkMessage::SendBlocksByRootChunk {
                                peer_id,
                                inbound_request_id,
                                block: Some(block.clone()),
                            });
                    }
                }

                output = output.with_network_message(NetworkMessage::SendBlocksByRootChunk {
                    peer_id,
                    inbound_request_id,
                    block: None,
                });

                output
            }
        }
    }
}
