use validator::ValidatorConfig;

#[test]
fn test_proposer_selection() {
    let config = ValidatorConfig {
        node_id: "test_0".to_string(),
        validator_indices: vec![2],
    };
    let num_validators: u64 = 4;

    // Proposer for a slot is determined by: slot % num_validators.
    let should_propose = |slot: u64| -> bool {
        let proposer_index = slot % num_validators;
        config.is_assigned(proposer_index)
    };

    // Validator 2 should propose at slots 2, 6, 10, ...
    assert!(should_propose(2));
    assert!(should_propose(6));
    assert!(should_propose(10));

    // Validator 2 should NOT propose at slots 0, 1, 3, 4, 5, ...
    assert!(!should_propose(0));
    assert!(!should_propose(1));
    assert!(!should_propose(3));
    assert!(!should_propose(4));
    assert!(!should_propose(5));
}

#[test]
fn test_is_assigned() {
    let config = ValidatorConfig {
        node_id: "test_0".to_string(),
        validator_indices: vec![2, 5, 8],
    };

    assert!(config.is_assigned(2));
    assert!(config.is_assigned(5));
    assert!(config.is_assigned(8));
    assert!(!config.is_assigned(0));
    assert!(!config.is_assigned(1));
    assert!(!config.is_assigned(3));
}
