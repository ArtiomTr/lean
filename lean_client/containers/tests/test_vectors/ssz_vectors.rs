use std::{collections::HashMap, path::Path};

use containers::{
    AggregatedAttestation, Attestation, AttestationData, Block, BlockBody, BlockHeader,
    BlockSignatures, BlockWithAttestation, Checkpoint, Config, PublicKey, SecretKey, Signature,
    SignedAttestation, SignedBlockWithAttestation, State, Status, Validator,
};
use serde::Deserialize;
use serde_json::Value;
use ssz::{SszReadDefault, SszWrite};
use test_generator::test_resources;

#[derive(Debug, Deserialize)]
struct SszVectorFile {
    #[serde(flatten)]
    tests: HashMap<String, SszTestCase>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SszTestCase {
    type_name: String,
    value: Value,
    serialized: String,
}

macro_rules! assert_ssz_case {
    ($ty:ty, $test_name:expr, $value:expr, $expected:expr) => {{
        let expected_bytes: &[u8] = $expected.as_ref();

        let parsed: $ty = serde_json::from_value($value).unwrap_or_else(|err| {
            panic!("{}: failed to deserialize test value: {}", $test_name, err)
        });

        let encoded = parsed
            .to_ssz()
            .unwrap_or_else(|err| panic!("{}: failed to serialize to SSZ: {}", $test_name, err));
        assert_eq!(
            encoded, expected_bytes,
            "{}: SSZ serialization bytes mismatch",
            $test_name
        );

        let decoded = <$ty>::from_ssz_default(expected_bytes).unwrap_or_else(|err| {
            panic!(
                "{}: failed to decode expected bytes as SSZ: {}",
                $test_name, err
            )
        });

        assert_eq!(
            decoded.to_ssz().expect("serialization should succeed"),
            expected_bytes,
            "{}: SSZ re-serialization mismatch after decode",
            $test_name
        );
    }};
}

fn assert_ssz_decode_only_case<T>(test_name: &str, expected: &[u8])
where
    T: SszReadDefault + SszWrite,
{
    let decoded = T::from_ssz_default(expected).unwrap_or_else(|err| {
        panic!(
            "{}: failed to decode expected bytes as SSZ: {}",
            test_name, err
        )
    });

    assert_eq!(
        decoded.to_ssz().expect("serialization should succeed"),
        expected,
        "{}: SSZ re-serialization mismatch after decode",
        test_name
    );
}

#[test_resources("test_vectors/ssz/*/ssz/**/*.json")]
fn ssz_vectors(spec_file: &str) {
    let test_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(spec_file);

    let json_content = std::fs::read_to_string(test_path.as_path())
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", test_path.display()));

    let file: SszVectorFile = serde_json::from_str(&json_content)
        .unwrap_or_else(|err| panic!("failed to parse {}: {err}", test_path.display()));

    let (test_name, case) = file
        .tests
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("no test case found in {}", test_path.display()));

    let expected = decode_hex(&case.serialized)
        .unwrap_or_else(|err| panic!("{test_name} has invalid serialized hex: {err}"));

    match case.type_name.as_str() {
        "Attestation" => assert_ssz_case!(Attestation, &test_name, case.value, &expected),
        "AttestationData" => {
            assert_ssz_case!(AttestationData, &test_name, case.value, &expected)
        }
        "AggregatedAttestation" => {
            assert_ssz_case!(AggregatedAttestation, &test_name, case.value, &expected)
        }
        "SignedAttestation" => {
            assert_ssz_case!(SignedAttestation, &test_name, case.value, &expected)
        }
        "BlockBody" => assert_ssz_case!(BlockBody, &test_name, case.value, &expected),
        "BlockHeader" => assert_ssz_case!(BlockHeader, &test_name, case.value, &expected),
        "Block" => assert_ssz_case!(Block, &test_name, case.value, &expected),
        "BlockWithAttestation" => {
            assert_ssz_case!(BlockWithAttestation, &test_name, case.value, &expected)
        }
        "BlockSignatures" => {
            assert_ssz_case!(BlockSignatures, &test_name, case.value, &expected)
        }
        "SignedBlockWithAttestation" => {
            assert_ssz_case!(
                SignedBlockWithAttestation,
                &test_name,
                case.value,
                &expected
            )
        }
        "Checkpoint" => assert_ssz_case!(Checkpoint, &test_name, case.value, &expected),
        "Config" => assert_ssz_case!(Config, &test_name, case.value, &expected),
        "Validator" => assert_ssz_case!(Validator, &test_name, case.value, &expected),
        "State" => assert_ssz_case!(State, &test_name, case.value, &expected),
        "Status" => assert_ssz_case!(Status, &test_name, case.value, &expected),
        "PublicKey" => assert_ssz_decode_only_case::<PublicKey>(&test_name, &expected),
        "SecretKey" => assert_ssz_decode_only_case::<SecretKey>(&test_name, &expected),
        "Signature" => assert_ssz_decode_only_case::<Signature>(&test_name, &expected),
        other => panic!(
            "unsupported SSZ type '{}' in test {} ({})",
            other,
            test_name,
            test_path.display()
        ),
    }
}

fn decode_hex(value: &str) -> Result<Vec<u8>, String> {
    let hex_string = value.strip_prefix("0x").unwrap_or(value);
    hex::decode(hex_string).map_err(|err| err.to_string())
}
