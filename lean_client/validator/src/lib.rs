// Lean validator client with XMSS signing support
use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, anyhow};
use tracing::info;

pub mod keys;
mod service;

pub use keys::KeyManager;
pub use service::ValidatorService;

pub type ValidatorRegistry = HashMap<String, Vec<u64>>;

/// Configuration specifying which validators a node controls
#[derive(Debug, Clone)]
pub struct ValidatorConfig {
    pub node_id: String,
    pub validator_indices: Vec<u64>,
}

impl ValidatorConfig {
    /// Load validator configuration from a YAML registry file.
    pub fn load_from_file(path: impl AsRef<Path>, node_id: &str) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        let registry: ValidatorRegistry = serde_yaml::from_reader(file)?;

        let indices = registry
            .get(node_id)
            .ok_or_else(|| anyhow!("Node `{node_id}` not found in validator registry"))?
            .clone();

        info!(node_id = %node_id, indices = ?indices, "Validator config loaded");

        Ok(ValidatorConfig {
            node_id: node_id.to_string(),
            validator_indices: indices,
        })
    }

    /// Check if a validator index is assigned to this node.
    pub fn is_assigned(&self, index: u64) -> bool {
        self.validator_indices.contains(&index)
    }
}
