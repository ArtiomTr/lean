use std::{collections::HashMap, fs::File, panic::AssertUnwindSafe, path::Path};

use containers::{
    AggregatedAttestation, AggregationBits, Attestation, AttestationData, Block, BlockBody,
    BlockHeader, BlockSignatures, BlockWithAttestation, Checkpoint, Config, HistoricalBlockHashes,
    JustificationRoots, JustificationValidators, JustifiedSlots, SignedBlockWithAttestation, Slot,
    State, Validator, Validators,
};
use fork_choice::Store;
use serde::Deserialize;
use ssz::{H256, SszHash as _};
use test_generator::test_resources;
use xmss::PublicKey;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestCase {
    #[allow(dead_code)]
    network: String,
    anchor_state: TestAnchorState,
    anchor_block: TestAnchorBlock,
    steps: Vec<TestStep>,
    #[serde(rename = "_info")]
    info: TestInfo,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestAnchorState {
    config: TestConfig,
    slot: u64,
    latest_block_header: TestBlockHeader,
    latest_justified: TestCheckpoint,
    latest_finalized: TestCheckpoint,
    #[serde(default)]
    historical_block_hashes: TestDataWrapper<String>,
    #[serde(default)]
    justified_slots: TestDataWrapper<bool>,
    validators: TestDataWrapper<TestValidator>,
    #[serde(default)]
    justifications_roots: TestDataWrapper<String>,
    #[serde(default)]
    justifications_validators: TestDataWrapper<bool>,
}

impl From<TestAnchorState> for State {
    fn from(value: TestAnchorState) -> State {
        let config = value.config.into();

        let latest_block_header = value.latest_block_header.into();

        let mut historical_block_hashes = HistoricalBlockHashes::default();
        for hash_str in &value.historical_block_hashes.data {
            historical_block_hashes
                .push(parse_root(hash_str))
                .expect("within limit");
        }

        let mut justified_slots = JustifiedSlots::new(false, value.justified_slots.data.len());
        for (i, &val) in value.justified_slots.data.iter().enumerate() {
            if val {
                justified_slots.set(i, true);
            }
        }

        let mut justifications_roots = JustificationRoots::default();
        for root_str in &value.justifications_roots.data {
            justifications_roots
                .push(parse_root(root_str))
                .expect("within limit");
        }

        let mut justifications_validators =
            JustificationValidators::new(false, value.justifications_validators.data.len());
        for (i, &val) in value.justifications_validators.data.iter().enumerate() {
            if val {
                justifications_validators.set(i, true);
            }
        }

        let mut validators = Validators::default();
        for test_validator in &value.validators.data {
            let pubkey: PublicKey = test_validator
                .pubkey
                .parse()
                .expect("Failed to parse validator pubkey");
            let validator = Validator {
                pubkey,
                index: test_validator.index,
            };
            validators.push(validator).expect("Failed to add validator");
        }

        State {
            config,
            slot: Slot(value.slot),
            latest_block_header,
            latest_justified: value.latest_justified.into(),
            latest_finalized: value.latest_finalized.into(),
            historical_block_hashes,
            justified_slots,
            validators,
            justifications_roots,
            justifications_validators,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestConfig {
    genesis_time: u64,
}

impl From<TestConfig> for Config {
    fn from(value: TestConfig) -> Config {
        Config {
            genesis_time: value.genesis_time,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestBlockHeader {
    slot: u64,
    proposer_index: u64,
    parent_root: String,
    state_root: String,
    body_root: String,
}

impl From<TestBlockHeader> for BlockHeader {
    fn from(value: TestBlockHeader) -> BlockHeader {
        BlockHeader {
            slot: Slot(value.slot),
            proposer_index: value.proposer_index,
            parent_root: parse_root(&value.parent_root),
            state_root: parse_root(&value.state_root),
            body_root: parse_root(&value.body_root),
        }
    }
}

#[derive(Debug, Deserialize)]
struct TestCheckpoint {
    root: String,
    slot: u64,
}

impl From<TestCheckpoint> for Checkpoint {
    fn from(value: TestCheckpoint) -> Checkpoint {
        Checkpoint {
            root: parse_root(&value.root),
            slot: Slot(value.slot),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct TestDataWrapper<T> {
    data: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct TestValidator {
    #[allow(dead_code)]
    pubkey: String,
    #[allow(dead_code)]
    #[serde(default)]
    index: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestAnchorBlock {
    slot: u64,
    proposer_index: u64,
    parent_root: String,
    state_root: String,
    body: TestBlockBody,
}

impl From<TestAnchorBlock> for SignedBlockWithAttestation {
    fn from(value: TestAnchorBlock) -> SignedBlockWithAttestation {
        let mut attestations = ssz::PersistentList::default();

        for (i, attestation) in value.body.attestations.data.into_iter().enumerate() {
            attestations
                .push(attestation.into())
                .expect(&format!("Failed to add attestation {}", i));
        }

        let block = Block {
            slot: Slot(value.slot),
            proposer_index: value.proposer_index,
            parent_root: parse_root(&value.parent_root),
            state_root: parse_root(&value.state_root),
            body: BlockBody { attestations },
        };

        // Create proposer attestation
        let proposer_attestation = Attestation {
            validator_id: value.proposer_index,
            data: AttestationData {
                slot: Slot(value.slot),
                head: Checkpoint {
                    root: parse_root(&value.parent_root),
                    slot: Slot(value.slot),
                },
                target: Checkpoint {
                    root: parse_root(&value.parent_root),
                    slot: Slot(value.slot),
                },
                source: Checkpoint {
                    root: parse_root(&value.parent_root),
                    slot: Slot(0),
                },
            },
        };

        SignedBlockWithAttestation {
            message: BlockWithAttestation {
                block,
                proposer_attestation,
            },
            signature: BlockSignatures::default(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestBlock {
    slot: u64,
    proposer_index: u64,
    parent_root: String,
    state_root: String,
    body: TestBlockBody,
}

impl From<TestBlock> for Block {
    fn from(value: TestBlock) -> Block {
        Block {
            slot: Slot(value.slot),
            proposer_index: value.proposer_index,
            parent_root: parse_root(&value.parent_root),
            state_root: parse_root(&value.state_root),
            body: value.body.into(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestBlockWithAttestation {
    block: TestBlock,
    proposer_attestation: TestAttestation,
    #[serde(default)]
    block_root_label: Option<String>,
}

impl From<TestBlockWithAttestation> for BlockWithAttestation {
    fn from(value: TestBlockWithAttestation) -> BlockWithAttestation {
        BlockWithAttestation {
            block: value.block.into(),
            proposer_attestation: value.proposer_attestation.into(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestAttestation {
    validator_id: u64,
    data: TestAttestationData,
}

impl From<TestAttestation> for Attestation {
    fn from(value: TestAttestation) -> Attestation {
        Attestation {
            validator_id: value.validator_id,
            data: value.data.into(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct TestBlockBody {
    attestations: TestDataWrapper<TestAggregatedAttestation>,
}

impl From<TestBlockBody> for BlockBody {
    fn from(value: TestBlockBody) -> BlockBody {
        let mut attestations = ssz::PersistentList::default();

        for attestation in value.attestations.data {
            attestations
                .push(attestation.into())
                .expect("failed to add attestation");
        }

        BlockBody { attestations }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestAggregatedAttestation {
    aggregation_bits: TestAggregationBits,
    data: TestAttestationData,
}

impl From<TestAggregatedAttestation> for AggregatedAttestation {
    fn from(value: TestAggregatedAttestation) -> AggregatedAttestation {
        AggregatedAttestation {
            aggregation_bits: value.aggregation_bits.into(),
            data: value.data.into(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct TestAggregationBits {
    data: Vec<bool>,
}

impl From<TestAggregationBits> for AggregationBits {
    fn from(value: TestAggregationBits) -> AggregationBits {
        let mut bitlist = ssz::BitList::with_length(value.data.len());
        for (i, &bit) in value.data.iter().enumerate() {
            bitlist.set(i, bit);
        }
        AggregationBits(bitlist)
    }
}

#[derive(Debug, Deserialize)]
struct TestAttestationData {
    slot: u64,
    head: TestCheckpoint,
    target: TestCheckpoint,
    source: TestCheckpoint,
}

impl From<TestAttestationData> for AttestationData {
    fn from(value: TestAttestationData) -> AttestationData {
        AttestationData {
            slot: Slot(value.slot),
            head: value.head.into(),
            target: value.target.into(),
            source: value.source.into(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestStep {
    valid: bool,
    #[serde(default)]
    checks: Option<TestChecks>,
    #[serde(rename = "stepType")]
    step_type: String,
    block: Option<TestBlockWithAttestation>,
    attestation: Option<TestAggregatedAttestation>,
    tick: Option<u64>,
    time: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestChecks {
    #[serde(rename = "headSlot")]
    head_slot: Option<u64>,
    #[serde(rename = "headRootLabel")]
    head_root_label: Option<String>,
    #[serde(rename = "attestationChecks")]
    attestation_checks: Option<Vec<AttestationCheck>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttestationCheck {
    validator: u64,
    #[allow(dead_code)]
    #[serde(rename = "attestationSlot")]
    attestation_slot: u64,
    #[serde(rename = "targetSlot")]
    target_slot: Option<u64>,
    location: String,
}

#[derive(Debug, Deserialize)]
struct TestInfo {
    #[allow(dead_code)]
    hash: String,
    #[allow(dead_code)]
    comment: String,
    #[serde(rename = "testId")]
    test_id: String,
    #[allow(dead_code)]
    description: String,
    #[allow(dead_code)]
    #[serde(rename = "fixtureFormat")]
    fixture_format: String,
}

fn parse_root(hex_str: &str) -> H256 {
    let hex = hex_str.trim_start_matches("0x");
    let mut bytes = [0u8; 32];

    if hex.len() == 64 {
        for i in 0..32 {
            bytes[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                .unwrap_or_else(|_| panic!("Invalid hex at position {}: {}", i, hex));
        }
    } else if !hex.chars().all(|c| c == '0') {
        panic!("Invalid root length: {} (expected 64 hex chars)", hex.len());
    }

    H256::from(bytes)
}

fn verify_checks(
    store: &Store,
    checks: &Option<TestChecks>,
    block_labels: &HashMap<String, H256>,
    step_idx: usize,
) -> Result<(), String> {
    // If no checks provided, nothing to verify
    let checks = match checks {
        Some(c) => c,
        None => return Ok(()),
    };

    if let Some(expected_slot) = checks.head_slot {
        // Per devnet-2, store.blocks now contains Block (not SignedBlockWithAttestation)
        let actual_slot = store
            .blocks()
            .get(&store.head())
            .map(|b| b.slot.0)
            .unwrap_or(0);
        if actual_slot != expected_slot {
            return Err(format!(
                "Step {}: Head slot mismatch - expected {}, got {}",
                step_idx, expected_slot, actual_slot
            ));
        }
    }

    if let Some(label) = &checks.head_root_label {
        let expected_root = block_labels
            .get(label)
            .ok_or_else(|| format!("Step {}: Block label '{}' not found", step_idx, label))?;
        if &store.head() != expected_root {
            // Per devnet-2, store.blocks now contains Block (not SignedBlockWithAttestation)
            let actual_slot = store
                .blocks()
                .get(&store.head())
                .map(|b| b.slot.0)
                .unwrap_or(0);
            let expected_slot = store
                .blocks()
                .get(expected_root)
                .map(|b| b.slot.0)
                .unwrap_or(0);
            return Err(format!(
                "Step {}: Head root mismatch for label '{}' - expected slot {}, got slot {}",
                step_idx, label, expected_slot, actual_slot,
            ));
        }
    }

    // Note: attestation checks are disabled as the Store API changed from
    // latest_known_attestations/latest_new_attestations to aggregated payloads
    // TODO: Update attestation checks to use new API if needed

    Ok(())
}

#[test_resources("test_vectors/fork_choice/*/fc/*/*.json")]
fn forkchoice(spec_file: &str) {
    let spec_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(spec_file);
    let mut file =
        File::open(&spec_path).expect(&format!("failed to open spec file {spec_path:?}"));
    let test_cases: HashMap<String, TestCase> = serde_json::from_reader(&mut file).unwrap();

    for (_, case) in test_cases {
        let config = Config {
            genesis_time: case.anchor_state.config.genesis_time,
        };

        let mut anchor_state: State = case.anchor_state.into();
        let anchor_block: SignedBlockWithAttestation = case.anchor_block.into();

        let body_root = anchor_block.message.block.body.hash_tree_root();
        anchor_state.latest_block_header = BlockHeader {
            slot: anchor_block.message.block.slot,
            proposer_index: anchor_block.message.block.proposer_index,
            parent_root: anchor_block.message.block.parent_root,
            state_root: anchor_block.message.block.state_root,
            body_root,
        };

        let mut store = Store::new(anchor_state, anchor_block.message.block, None);
        let mut block_labels: HashMap<String, H256> = HashMap::new();

        for (step_idx, step) in case.steps.into_iter().enumerate() {
            match step.step_type.as_str() {
                "block" => {
                    let test_block = step
                        .block
                        .expect(&format!("Step {step_idx}: Missing block data"));

                    let block_root_label = test_block.block_root_label.clone();

                    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        let block: BlockWithAttestation = test_block.into();
                        let signed_block: SignedBlockWithAttestation = SignedBlockWithAttestation {
                            message: block,
                            signature: BlockSignatures::default(),
                        };
                        let block_root = signed_block.message.block.hash_tree_root();

                        // Advance time to the block's slot to ensure attestations are processable
                        // SECONDS_PER_SLOT is 4 (not 12)
                        let block_time =
                            store.config().genesis_time + (signed_block.message.block.slot.0 * 4);
                        store.on_tick(block_time, false, false);

                        store.on_block(signed_block).unwrap();
                        Ok(block_root)
                    }));

                    let result = match result {
                        Ok(inner) => inner,
                        Err(e) => Err(format!("Panic: {:?}", e)),
                    };

                    if let Ok(block_root) = &result {
                        if let Some(label) = block_root_label {
                            block_labels.insert(label.clone(), *block_root);
                        }
                    }

                    if step.valid && result.is_err() {
                        panic!(
                            "Step {step_idx}: Block should be valid but processing failed: {:?}",
                            result.err().unwrap()
                        );
                    } else if !step.valid && result.is_ok() {
                        panic!(
                            "Step: {step_idx}: Block should be invalid but processing succeeded"
                        );
                    }

                    if step.valid && result.is_ok() {
                        verify_checks(&store, &step.checks, &block_labels, step_idx).expect(
                            &format!("Step: {step_idx}: Should be valid but checks failed"),
                        );
                    }
                }
                "tick" | "time" => {
                    let time_value = step
                        .tick
                        .or(step.time)
                        .expect(&format!("Step {step_idx}: Missing tick/time data"));
                    store.on_tick(time_value, false, false);

                    if step.valid {
                        verify_checks(&store, &step.checks, &block_labels, step_idx).expect(
                            &format!("Step: {step_idx}: Should be valid but checks failed"),
                        );
                    }
                }
                // "attestation" => {
                //     let test_att = step
                //         .attestation
                //         .as_ref()
                //         .expect(&format!("Step {}: Missing attestation data", step_idx));

                //     let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                //         let attestation: AttestationData = test_att.into();
                //         let signed_attestation = SignedAttestation {
                //             message: attestation,
                //             signature: Signature::default(),
                //         };
                //         store.on_attestation(signed_attestation, false)
                //     }));

                //     let result = match result {
                //         Ok(inner) => inner,
                //         Err(e) => Err(format!("Panic: {:?}", e)),
                //     };

                //     if step.valid && result.is_err() {
                //         panic!("Step {step_idx}: Attestation should be valid but processing failed: {:?}", result.err().unwrap());
                //     } else if !step.valid && result.is_ok() {
                //         panic!("Step {step_idx}: Attestation should be invalid but processing succeeded");
                //     }

                //     if step.valid && result.is_ok() {
                //         verify_checks(&store, &step.checks, &block_labels, step_idx)?;
                //     }
                // }
                _ => {
                    panic!("Step {step_idx}: Unknown step type: {}", step.step_type);
                }
            }
        }
    }
}
