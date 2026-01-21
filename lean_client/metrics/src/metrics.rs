use std::{sync::Arc, time::SystemTime};

use anyhow::{Context, Result};
use once_cell::sync::OnceCell;
use prometheus::{GaugeVec, Histogram, IntCounter, IntGauge, histogram_opts, opts};

pub static METRICS: OnceCell<Arc<Metrics>> = OnceCell::new();

#[derive(Debug)]
pub struct Metrics {
    /// Node information: name and version
    lean_node_info: GaugeVec,

    /// Start timestamp
    lean_node_start_time_seconds: IntGauge,

    // PQ Signature metrics
    /// Time taken to sign an attestation
    pub lean_pq_sig_attestation_signing_time_seconds: Histogram,

    /// Time taken to verify an attestation signature
    pub lean_pq_sig_attestation_verification_time_seconds: Histogram,

    /// Total number of aggregated signatures
    lean_pq_sig_aggregated_signatures_total: IntCounter,

    /// Total number of attestations included into aggregated signatures
    lean_pq_sig_attestations_in_aggregated_signatures_total: IntCounter,

    /// Time taken to verify an aggregated attestation signature
    lean_pq_sig_attestation_signatures_building_time_seconds: Histogram,

    /// Time taken to verify an aggregated attestation signature
    lean_pq_sig_aggregated_signatures_verification_time_seconds: Histogram,

    /// Total number of valid aggregated signatures
    lean_pq_sig_aggregated_signatures_valid_total: IntCounter,

    /// Total number of invalid aggregated signatures
    lean_pq_sig_aggregated_signatures_invalid_total: IntCounter,

    // Fork-Choice Metrics
    /// Latest slot of the lean chain
    pub lean_head_slot: IntGauge,

    /// Current slot of the lean chain
    lean_current_slot: IntGauge,

    /// Safe target slot
    pub lean_safe_target_slot: IntGauge,

    /// Time taken to process block
    pub lean_fork_choice_block_processing_time_seconds: Histogram,

    /// Total number of valid attestations
    lean_attestations_valid_total: IntCounter,

    /// Total number of invalid attestations
    lean_attestations_invalid_total: IntCounter,

    /// Time taken to validate attestation
    lean_attestation_validation_time_seconds: Histogram,

    /// Total number of fork choice reorgs
    lean_fork_choice_reorgs_total: IntCounter,

    /// Depth of fork choice reorgs (in blocks)
    lean_fork_choice_reorg_depth: Histogram,

    // State Transition Metrics
    /// Latest justified slot
    lean_latest_justified_slot: IntGauge,

    /// Latest finalized slot
    lean_latest_finalized_slot: IntGauge,

    /// Total number of finalization attempts
    lean_finalizations_total: IntCounter,

    /// Time to process state transition
    lean_state_transition_time_seconds: Histogram,

    /// Total number of processed slots
    lean_state_transition_slots_processed_total: IntCounter,

    /// Time taken to process slots
    lean_state_transition_slots_processing_time_seconds: Histogram,

    /// Time taken to process block
    lean_state_transition_block_processing_time_seconds: Histogram,

    /// Total number of processed attestations
    lean_state_transition_attestations_processed_total: IntCounter,

    /// Time taken to process attestations
    lean_state_transition_attestations_processing_time_seconds: Histogram,

    // Validator metrics
    /// Number of validators managed by a node
    lean_validators_count: IntGauge,

    // Network Metrics
    /// Number of connected peers
    lean_connected_peers: IntGauge,

    /// Total number of peer connection events
    lean_peer_connection_events_total: IntCounter,

    /// Total number of peer disconnection events
    lean_peer_disconnection_events_total: IntCounter,
}

impl Metrics {
    pub fn new() -> Result<Self> {
        Ok(Self {
            lean_node_info: GaugeVec::new(
                opts!("lean_node_info", "Node information"),
                &["name", "version"],
            )?,
            lean_node_start_time_seconds: IntGauge::new(
                "lean_node_start_time_seconds",
                "Start timestamp",
            )?,

            // PQ Signature metrics
            lean_pq_sig_attestation_signing_time_seconds: Histogram::with_opts(histogram_opts!(
                "lean_pq_sig_attestation_signing_time_seconds",
                "Time taken to sign an attestation",
                vec![0.005, 0.01, 0.025, 0.05, 0.1, 1.0],
            ))?,
            lean_pq_sig_attestation_verification_time_seconds: Histogram::with_opts(
                histogram_opts!(
                    "lean_pq_sig_attestation_verification_time_seconds",
                    "Time taken to verify an attestation signature",
                    vec![0.005, 0.01, 0.025, 0.05, 0.1, 1.0],
                ),
            )?,
            lean_pq_sig_aggregated_signatures_total: IntCounter::new(
                "lean_pq_sig_aggregated_signatures_total",
                "Total number of aggregated signatures",
            )?,
            lean_pq_sig_attestations_in_aggregated_signatures_total: IntCounter::new(
                "lean_pq_sig_attestations_in_aggregated_signatures_total",
                "Total number of attestations included into aggregated signatures",
            )?,
            lean_pq_sig_attestation_signatures_building_time_seconds: Histogram::with_opts(
                histogram_opts!(
                    "lean_pq_sig_attestation_signatures_building_time_seconds",
                    "Time taken to verify an aggregated attestation signature",
                    vec![0.005, 0.01, 0.025, 0.05, 0.1, 1.0]
                ),
            )?,
            lean_pq_sig_aggregated_signatures_verification_time_seconds: Histogram::with_opts(
                histogram_opts!(
                    "lean_pq_sig_aggregated_signatures_verification_time_seconds",
                    "Time taken to verify an aggregated attestation signature",
                    vec![0.005, 0.01, 0.025, 0.05, 0.1, 1.0]
                ),
            )?,
            lean_pq_sig_aggregated_signatures_valid_total: IntCounter::new(
                "lean_pq_sig_aggregated_signatures_valid_total",
                "On validate aggregated signature",
            )?,
            lean_pq_sig_aggregated_signatures_invalid_total: IntCounter::new(
                "lean_pq_sig_aggregated_signatures_invalid_total",
                "Total number of invalid aggregated signatures",
            )?,

            // Fork-Choice Metrics
            lean_head_slot: IntGauge::new("lean_head_slot", "Latest slot of the lean chain")?,
            lean_current_slot: IntGauge::new(
                "lean_current_slot",
                "Current slot of the lean chain",
            )?,
            lean_safe_target_slot: IntGauge::new("lean_safe_target_slot", "Safe target slot")?,
            lean_fork_choice_block_processing_time_seconds: Histogram::with_opts(histogram_opts!(
                "lean_fork_choice_block_processing_time_seconds",
                "Time taken to process block",
                vec![0.005, 0.01, 0.025, 0.05, 0.1, 1.0]
            ))?,
            lean_attestations_valid_total: IntCounter::new(
                "lean_attestations_valid_total",
                "Total number of valid attestations",
            )?,
            lean_attestations_invalid_total: IntCounter::new(
                "lean_attestations_invalid_total",
                "Total number of invalid attestations",
            )?,
            lean_attestation_validation_time_seconds: Histogram::with_opts(histogram_opts!(
                "lean_attestation_validation_time_seconds",
                "Time taken to validate attestation",
                vec![0.005, 0.01, 0.025, 0.05, 0.1, 1.0]
            ))?,
            lean_fork_choice_reorgs_total: IntCounter::new(
                "lean_fork_choice_reorgs_total",
                "Total number of fork choice reorgs",
            )?,
            lean_fork_choice_reorg_depth: Histogram::with_opts(histogram_opts!(
                "lean_fork_choice_reorg_depth",
                "Depth of fork choice reorgs (in blocks)",
                vec![1.0, 2.0, 3.0, 5.0, 7.0, 10.0, 20.0, 30.0, 50.0, 100.0]
            ))?,

            // State Transition Metrics
            lean_latest_justified_slot: IntGauge::new(
                "lean_latest_justified_slot",
                "Latest justified slot",
            )?,
            lean_latest_finalized_slot: IntGauge::new(
                "lean_latest_finalized_slot",
                "Latest finalized slot",
            )?,
            lean_finalizations_total: IntCounter::new(
                "lean_finalizations_total",
                "Total number of finalization attempts",
            )?,
            lean_state_transition_time_seconds: Histogram::with_opts(histogram_opts!(
                "lean_state_transition_time_seconds",
                "Time to process state transition",
                vec![0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 2.5, 3.0, 4.0]
            ))?,
            lean_state_transition_slots_processed_total: IntCounter::new(
                "lean_state_transition_slots_processed_total",
                "Total number of processed slots",
            )?,
            lean_state_transition_slots_processing_time_seconds: Histogram::with_opts(
                histogram_opts!(
                    "lean_state_transition_slots_processing_time_seconds",
                    "Time taken to process slots",
                    vec![0.005, 0.01, 0.025, 0.05, 0.1, 1.0]
                ),
            )?,
            lean_state_transition_block_processing_time_seconds: Histogram::with_opts(
                histogram_opts!(
                    "lean_state_transition_block_processing_time_seconds",
                    "Time taken to process block",
                    vec![0.005, 0.01, 0.025, 0.05, 0.1, 1.0]
                ),
            )?,
            lean_state_transition_attestations_processed_total: IntCounter::new(
                "lean_state_transition_attestations_processed_total",
                "Total number of processed attestations",
            )?,
            lean_state_transition_attestations_processing_time_seconds: Histogram::with_opts(
                histogram_opts!(
                    "lean_state_transition_attestations_processing_time_seconds",
                    " Time taken to process attestations",
                    vec![0.005, 0.01, 0.025, 0.05, 0.1, 1.0]
                ),
            )?,

            // Validator metrics
            lean_validators_count: IntGauge::new(
                "lean_validators_count",
                "Number of validators managed by a node",
            )?,

            // Network Metrics
            lean_connected_peers: IntGauge::new(
                "lean_connected_peers",
                "Number of connected peers",
            )?,
            lean_peer_connection_events_total: IntCounter::new(
                "lean_peer_connection_events_total",
                "Total number of peer connection events",
            )?,
            lean_peer_disconnection_events_total: IntCounter::new(
                "lean_peer_disconnection_events_total",
                "Total number of peer disconnection events",
            )?,
        })
    }

    pub fn register_with_default_metrics(&self) -> Result<()> {
        let default_registry = prometheus::default_registry();

        default_registry.register(Box::new(self.lean_node_info.clone()))?;
        default_registry.register(Box::new(self.lean_node_start_time_seconds.clone()))?;
        default_registry.register(Box::new(
            self.lean_pq_sig_attestation_signing_time_seconds.clone(),
        ))?;
        default_registry.register(Box::new(
            self.lean_pq_sig_attestation_verification_time_seconds
                .clone(),
        ))?;
        default_registry.register(Box::new(
            self.lean_pq_sig_aggregated_signatures_total.clone(),
        ))?;
        default_registry.register(Box::new(
            self.lean_pq_sig_attestations_in_aggregated_signatures_total
                .clone(),
        ))?;
        default_registry.register(Box::new(
            self.lean_pq_sig_attestation_signatures_building_time_seconds
                .clone(),
        ))?;
        default_registry.register(Box::new(
            self.lean_pq_sig_aggregated_signatures_verification_time_seconds
                .clone(),
        ))?;
        default_registry.register(Box::new(
            self.lean_pq_sig_aggregated_signatures_valid_total.clone(),
        ))?;
        default_registry.register(Box::new(
            self.lean_pq_sig_aggregated_signatures_invalid_total.clone(),
        ))?;
        default_registry.register(Box::new(self.lean_head_slot.clone()))?;
        default_registry.register(Box::new(self.lean_current_slot.clone()))?;
        default_registry.register(Box::new(self.lean_safe_target_slot.clone()))?;
        default_registry.register(Box::new(
            self.lean_fork_choice_block_processing_time_seconds.clone(),
        ))?;
        default_registry.register(Box::new(self.lean_attestations_valid_total.clone()))?;
        default_registry.register(Box::new(self.lean_attestations_invalid_total.clone()))?;
        default_registry.register(Box::new(
            self.lean_attestation_validation_time_seconds.clone(),
        ))?;
        default_registry.register(Box::new(self.lean_fork_choice_reorgs_total.clone()))?;
        default_registry.register(Box::new(self.lean_fork_choice_reorg_depth.clone()))?;
        default_registry.register(Box::new(self.lean_latest_justified_slot.clone()))?;
        default_registry.register(Box::new(self.lean_latest_finalized_slot.clone()))?;
        default_registry.register(Box::new(self.lean_finalizations_total.clone()))?;
        default_registry.register(Box::new(self.lean_state_transition_time_seconds.clone()))?;
        default_registry.register(Box::new(
            self.lean_state_transition_slots_processed_total.clone(),
        ))?;
        default_registry.register(Box::new(
            self.lean_state_transition_slots_processing_time_seconds
                .clone(),
        ))?;
        default_registry.register(Box::new(
            self.lean_state_transition_block_processing_time_seconds
                .clone(),
        ))?;
        default_registry.register(Box::new(
            self.lean_state_transition_attestations_processed_total
                .clone(),
        ))?;
        default_registry.register(Box::new(
            self.lean_state_transition_attestations_processing_time_seconds
                .clone(),
        ))?;
        default_registry.register(Box::new(self.lean_validators_count.clone()))?;
        default_registry.register(Box::new(self.lean_connected_peers.clone()))?;
        default_registry.register(Box::new(self.lean_peer_connection_events_total.clone()))?;
        default_registry.register(Box::new(self.lean_peer_disconnection_events_total.clone()))?;

        Ok(())
    }

    pub fn set_client_version(&self, name: String, version: String) {
        self.lean_node_info
            .with_label_values(&[name, version])
            .set(1.0);
    }

    pub fn set_start_time(&self, timestamp: SystemTime) -> Result<()> {
        let timestamp = timestamp
            .duration_since(SystemTime::UNIX_EPOCH)
            .context("failed to calculate timestamp")?
            .as_secs()
            .try_into()
            .context("sorry, grandine support ended ~292 billion years ago")?;
        self.lean_node_start_time_seconds.set(timestamp);

        Ok(())
    }
}
