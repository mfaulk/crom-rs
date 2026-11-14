use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::bash_policy::BashCheckpointPolicy;
use crate::types::CheckpointLimits;

/// Behavior when checkpoint capture fails for a tracked tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointFailureMode {
    /// Default. If snapshot capture fails, the mutating tool is denied.
    #[default]
    Strict,
    /// Best effort: emit a warning, allow mutation to proceed.
    BestEffort,
}

impl CheckpointFailureMode {
    pub fn from_str_lenient(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "strict" => Some(Self::Strict),
            "best_effort" | "best-effort" | "besteffort" => Some(Self::BestEffort),
            _ => None,
        }
    }
}

/// Aggregate options that drive checkpointing for a single agent run.
#[derive(Debug, Clone)]
pub struct CheckpointOptions {
    pub enabled: bool,
    pub failure_mode: CheckpointFailureMode,
    pub create_on_user_message: bool,
    pub limits: CheckpointLimits,
    pub bash_policy: BashCheckpointPolicy,
}

impl Default for CheckpointOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            failure_mode: CheckpointFailureMode::Strict,
            create_on_user_message: true,
            limits: CheckpointLimits::default(),
            bash_policy: BashCheckpointPolicy::default(),
        }
    }
}

/// Validation command configuration for a single run.
#[derive(Debug, Clone)]
pub struct ValidationOptions {
    pub commands: Vec<String>,
    pub timeout: Duration,
    pub rewind_on_failure: bool,
}

impl Default for ValidationOptions {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            timeout: Duration::from_secs(60),
            rewind_on_failure: false,
        }
    }
}

impl ValidationOptions {
    pub fn is_enabled(&self) -> bool {
        !self.commands.is_empty()
    }
}
