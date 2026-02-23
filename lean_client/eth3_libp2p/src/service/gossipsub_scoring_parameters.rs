use crate::TopicHash;
use crate::types::{GossipEncoding, GossipKind, GossipTopic};
use gossipsub::{IdentTopic as Topic, PeerScoreParams, PeerScoreThresholds, TopicScoreParams};
use std::cmp::max;
use std::collections::HashMap;
use std::time::Duration;
use typenum::Unsigned as _;
use types::{
    config::Config as ChainConfig,
    phase0::{consts::AttestationSubnetCount, primitives::{ForkDigest, Slot}},
};

const MAX_IN_MESH_SCORE: f64 = 10.0;
const MAX_FIRST_MESSAGE_DELIVERIES_SCORE: f64 = 40.0;
const BEACON_BLOCK_WEIGHT: f64 = 0.5;
const BEACON_AGGREGATE_PROOF_WEIGHT: f64 = 0.5;

/// Slots per epoch for the lean client (fixed at 32).
const SLOTS_PER_EPOCH: u64 = 32;

/// The time window (seconds) that we expect messages to be forwarded to us in the mesh.
const MESH_MESSAGE_DELIVERIES_WINDOW: u64 = 2;

// Const as this is used in the peer manager to prevent gossip from disconnecting peers.
pub const GREYLIST_THRESHOLD: f64 = -16000.0;

/// Builds the peer score thresholds.
pub fn peer_gossip_thresholds() -> PeerScoreThresholds {
    PeerScoreThresholds {
        gossip_threshold: -4000.0,
        publish_threshold: -8000.0,
        graylist_threshold: GREYLIST_THRESHOLD,
        accept_px_threshold: 100.0,
        opportunistic_graft_threshold: 5.0,
    }
}

pub struct PeerScoreSettings {
    slot: Duration,
    epoch: Duration,

    beacon_attestation_subnet_weight: f64,
    max_positive_score: f64,

    decay_interval: Duration,
    decay_to_zero: f64,

    mesh_n: usize,
    attestation_subnet_count: u64,
}

impl PeerScoreSettings {
    pub fn new(chain_config: &ChainConfig, mesh_n: usize) -> PeerScoreSettings {
        let slot = chain_config.slot_duration_ms;
        let beacon_attestation_subnet_weight = 1.0 / AttestationSubnetCount::U64 as f64;
        let max_positive_score = (MAX_IN_MESH_SCORE + MAX_FIRST_MESSAGE_DELIVERIES_SCORE)
            * (BEACON_BLOCK_WEIGHT
                + BEACON_AGGREGATE_PROOF_WEIGHT
                + beacon_attestation_subnet_weight * AttestationSubnetCount::U64 as f64);

        PeerScoreSettings {
            slot,
            epoch: slot * SLOTS_PER_EPOCH as u32,
            beacon_attestation_subnet_weight,
            max_positive_score,
            decay_interval: max(Duration::from_secs(1), slot),
            decay_to_zero: 0.01,
            mesh_n,
            attestation_subnet_count: AttestationSubnetCount::U64,
        }
    }

    pub fn get_peer_score_params(
        &self,
        active_validators: u64,
        thresholds: &PeerScoreThresholds,
        current_slot: Slot,
    ) -> PeerScoreParams {
        let mut params = PeerScoreParams {
            decay_interval: self.decay_interval,
            decay_to_zero: self.decay_to_zero,
            retain_score: self.epoch * 100,
            app_specific_weight: 1.0,
            ip_colocation_factor_threshold: 8.0,
            behaviour_penalty_threshold: 6.0,
            behaviour_penalty_decay: self.score_parameter_decay(self.epoch * 10),
            slow_peer_decay: 0.1,
            slow_peer_weight: -10.0,
            slow_peer_threshold: 0.0,
            ..Default::default()
        };

        let target_value = Self::decay_convergence(
            params.behaviour_penalty_decay,
            10.0 / SLOTS_PER_EPOCH as f64,
        ) - params.behaviour_penalty_threshold;
        params.behaviour_penalty_weight = thresholds.gossip_threshold / target_value.powi(2);

        params.topic_score_cap = self.max_positive_score * 0.5;
        params.ip_colocation_factor_weight = -params.topic_score_cap;

        params.topics = HashMap::new();

        let get_hash = |kind: GossipKind| -> TopicHash {
            let topic: Topic = GossipTopic::new(kind, GossipEncoding::default(), ForkDigest::default()).into();
            topic.hash()
        };

        let (beacon_block_params, beacon_aggregate_proof_params, beacon_attestation_subnet_params) =
            self.get_dynamic_topic_params(active_validators, current_slot);

        params
            .topics
            .insert(get_hash(GossipKind::BeaconBlock), beacon_block_params);

        params.topics.insert(
            get_hash(GossipKind::BeaconAggregateAndProof),
            beacon_aggregate_proof_params,
        );

        for i in 0..self.attestation_subnet_count {
            params.topics.insert(
                get_hash(GossipKind::Attestation(i)),
                beacon_attestation_subnet_params.clone(),
            );
        }

        params
    }

    pub fn get_dynamic_topic_params(
        &self,
        active_validators: u64,
        current_slot: Slot,
    ) -> (TopicScoreParams, TopicScoreParams, TopicScoreParams) {
        // Expected attestations per slot per subnet
        let attestations_per_subnet_per_slot =
            active_validators as f64 / self.attestation_subnet_count as f64 / SLOTS_PER_EPOCH as f64;

        // Expected aggregate proofs per slot
        let aggregates_per_slot = active_validators as f64 / SLOTS_PER_EPOCH as f64;

        let beacon_block_params = Self::get_topic_params(
            self,
            BEACON_BLOCK_WEIGHT,
            1.0,
            self.epoch * 20,
            Some((SLOTS_PER_EPOCH * 5, 3.0, self.epoch, current_slot)),
        );

        let beacon_aggregate_proof_params = Self::get_topic_params(
            self,
            BEACON_AGGREGATE_PROOF_WEIGHT,
            aggregates_per_slot,
            self.epoch,
            Some((SLOTS_PER_EPOCH * 2, 4.0, self.epoch, current_slot)),
        );

        let beacon_attestation_subnet_params = Self::get_topic_params(
            self,
            self.beacon_attestation_subnet_weight,
            attestations_per_subnet_per_slot,
            self.epoch * 4,
            Some((SLOTS_PER_EPOCH * 16, 16.0, self.epoch * 3, current_slot)),
        );

        (
            beacon_block_params,
            beacon_aggregate_proof_params,
            beacon_attestation_subnet_params,
        )
    }

    pub fn attestation_subnet_count(&self) -> u64 {
        self.attestation_subnet_count
    }

    fn decay_convergence(decay: f64, rate: f64) -> f64 {
        rate / (1.0 - decay)
    }

    fn threshold(decay: f64, rate: f64) -> f64 {
        Self::decay_convergence(decay, rate) * decay
    }

    fn score_parameter_decay(&self, decay_time: Duration) -> f64 {
        let ticks = decay_time.as_secs_f64() / self.decay_interval.as_secs_f64();
        self.decay_to_zero.powf(1.0 / ticks)
    }

    fn get_topic_params(
        &self,
        topic_weight: f64,
        expected_message_rate: f64,
        first_message_decay_time: Duration,
        mesh_message_info: Option<(u64, f64, Duration, Slot)>,
    ) -> TopicScoreParams {
        let mut t_params = TopicScoreParams::default();

        t_params.topic_weight = topic_weight;

        t_params.time_in_mesh_quantum = self.slot;
        t_params.time_in_mesh_cap = 3600.0 / t_params.time_in_mesh_quantum.as_secs_f64();
        t_params.time_in_mesh_weight = 10.0 / t_params.time_in_mesh_cap;

        t_params.first_message_deliveries_decay =
            self.score_parameter_decay(first_message_decay_time);
        t_params.first_message_deliveries_cap = Self::decay_convergence(
            t_params.first_message_deliveries_decay,
            2.0 * expected_message_rate / self.mesh_n as f64,
        );
        t_params.first_message_deliveries_weight = 40.0 / t_params.first_message_deliveries_cap;

        if let Some((decay_slots, cap_factor, activation_window, current_slot)) = mesh_message_info
        {
            let decay_time = self.slot * decay_slots as u32;
            t_params.mesh_message_deliveries_decay = self.score_parameter_decay(decay_time);
            t_params.mesh_message_deliveries_threshold = Self::threshold(
                t_params.mesh_message_deliveries_decay,
                expected_message_rate / 50.0,
            );
            t_params.mesh_message_deliveries_cap =
                if cap_factor * t_params.mesh_message_deliveries_threshold < 2.0 {
                    2.0
                } else {
                    cap_factor * t_params.mesh_message_deliveries_threshold
                };
            t_params.mesh_message_deliveries_activation = activation_window;
            t_params.mesh_message_deliveries_window =
                Duration::from_secs(MESH_MESSAGE_DELIVERIES_WINDOW);
            t_params.mesh_failure_penalty_decay = t_params.mesh_message_deliveries_decay;
            t_params.mesh_message_deliveries_weight = -t_params.topic_weight;
            t_params.mesh_failure_penalty_weight = t_params.mesh_message_deliveries_weight;
            if decay_slots >= current_slot {
                t_params.mesh_message_deliveries_threshold = 0.0;
                t_params.mesh_message_deliveries_weight = 0.0;
            }
        } else {
            t_params.mesh_message_deliveries_weight = 0.0;
            t_params.mesh_message_deliveries_threshold = 0.0;
            t_params.mesh_message_deliveries_decay = 0.0;
            t_params.mesh_message_deliveries_cap = 0.0;
            t_params.mesh_message_deliveries_window = Duration::from_secs(0);
            t_params.mesh_message_deliveries_activation = Duration::from_secs(0);
            t_params.mesh_failure_penalty_decay = 0.0;
            t_params.mesh_failure_penalty_weight = 0.0;
        }

        t_params.invalid_message_deliveries_weight =
            -self.max_positive_score / t_params.topic_weight;
        t_params.invalid_message_deliveries_decay = self.score_parameter_decay(self.epoch * 50);

        t_params
    }
}
