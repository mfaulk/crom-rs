//! Result schema v2 (M17.4): outcome taxonomy, reproducibility metadata, and
//! the comparability rule from `docs/milestones/MILESTONE_17.md` §8.2–8.3.
//! The JSON Schema in `schemas/result-v2.schema.json` is the contract for
//! downstream scoreboard/compare work; change it only with a version bump.

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::failure::FailureClass;
use crate::runner::{EvalResult, EvalStatus, RunConfig};
use crate::task::EvalTask;

/// Version written to every new `result.json` and `run.json`.
pub const RESULT_SCHEMA_VERSION: u32 = 2;

/// Results without a `schema_version` field predate v2.
pub const LEGACY_RESULT_SCHEMA_VERSION: u32 = 1;

/// JSON Schema (draft 2020-12) for a v2 `result.json`.
pub const RESULT_SCHEMA_JSON: &str = include_str!("../schemas/result-v2.schema.json");

/// Version of the harness-side prompt framing (the task prompt is sent
/// verbatim; retry/repair feedback is appended by the runner). Bump when the
/// runner changes how prompts are assembled.
pub const PROMPT_TEMPLATE_VERSION: &str = "eval-prompt-v1";

/// Fraction of provider/infra errors above which a run is unreliable.
pub const UNRELIABLE_ERROR_FRACTION: f64 = 0.10;

/// Top-level outcome class. Only `resolved` and `failed` enter the pass-rate
/// denominator; the rest are reported as counts beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Resolved,
    Failed,
    InfraError,
    ProviderError,
    Skipped,
    Unscorable,
}

impl Outcome {
    pub fn key(&self) -> &'static str {
        match self {
            Outcome::Resolved => "resolved",
            Outcome::Failed => "failed",
            Outcome::InfraError => "infra_error",
            Outcome::ProviderError => "provider_error",
            Outcome::Skipped => "skipped",
            Outcome::Unscorable => "unscorable",
        }
    }

    /// Derive the outcome from status, resolution, and primary failure class.
    /// Used for new results and to upgrade v1 results on read.
    pub fn derive(
        status: EvalStatus,
        resolved: bool,
        failure_class: Option<FailureClass>,
    ) -> Outcome {
        use FailureClass as F;
        if matches!(status, EvalStatus::Skipped) {
            return Outcome::Skipped;
        }
        if resolved {
            return Outcome::Resolved;
        }
        match failure_class {
            Some(F::ProviderError) => Outcome::ProviderError,
            Some(F::HarnessError | F::IsolationError | F::AdapterError) => Outcome::InfraError,
            Some(F::TaskInvalid | F::OracleValidationFailed | F::VerificationFlake) => {
                Outcome::Unscorable
            }
            _ => Outcome::Failed,
        }
    }

    /// Whether this outcome counts toward the pass-rate denominator.
    pub fn is_scored(&self) -> bool {
        matches!(self, Outcome::Resolved | Outcome::Failed)
    }
}

/// Per-outcome counts with the denominators defined in `docs/eval-schema.md`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeCounts {
    pub resolved: usize,
    pub failed: usize,
    pub provider_error: usize,
    pub infra_error: usize,
    pub skipped: usize,
    pub unscorable: usize,
}

impl OutcomeCounts {
    pub fn from_outcomes(outcomes: impl IntoIterator<Item = Outcome>) -> Self {
        let mut c = OutcomeCounts::default();
        for o in outcomes {
            match o {
                Outcome::Resolved => c.resolved += 1,
                Outcome::Failed => c.failed += 1,
                Outcome::ProviderError => c.provider_error += 1,
                Outcome::InfraError => c.infra_error += 1,
                Outcome::Skipped => c.skipped += 1,
                Outcome::Unscorable => c.unscorable += 1,
            }
        }
        c
    }

    /// Pass-rate denominator: `resolved + failed`.
    pub fn scored(&self) -> usize {
        self.resolved + self.failed
    }

    /// Attempts that ran or were meant to run (everything but `skipped`).
    pub fn attempted(&self) -> usize {
        self.scored() + self.provider_error + self.infra_error + self.unscorable
    }

    /// `resolved / (resolved + failed)`, or `None` with nothing scored.
    pub fn pass_rate(&self) -> Option<f64> {
        (self.scored() > 0).then(|| self.resolved as f64 / self.scored() as f64)
    }

    /// Provider + infra errors exceed 10% of attempts: do not compare.
    pub fn unreliable(&self) -> bool {
        let attempted = self.attempted();
        attempted > 0
            && (self.provider_error + self.infra_error) as f64 / attempted as f64
                > UNRELIABLE_ERROR_FRACTION
    }
}

/// Reproducibility metadata recorded on every v2 result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReproMetadata {
    pub git: GitMeta,
    pub task: TaskMeta,
    pub provider: ProviderMeta,
    pub tool_policy: ToolPolicyMeta,
    pub security: SecurityMeta,
    pub isolation: IsolationMeta,
    pub runtime: RuntimeMeta,
    pub prompt: PromptMeta,
    pub repetition: usize,
    pub seed: u64,
    pub host: HostInfo,
    /// Version of the crom build that produced the result.
    pub crom_version: String,
    /// Pricing table version used for `cost_usd`; `None` until pricing lands.
    pub pricing_source_version: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitMeta {
    pub sha: Option<String>,
    pub dirty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskMeta {
    pub id: String,
    pub schema_version: u32,
    /// Declared `task_version` (v3 manifests); `None` for v1/v2.
    pub content_version: Option<String>,
    /// `sha256:<hex>` over every file in the task directory.
    pub content_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderMeta {
    /// Provider mode or profile id (`fake`, `golden`, `deepseek`, ...).
    pub profile: String,
    pub model: String,
    /// Endpoint host only; never the full URL or credentials.
    pub base_url_host: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolPolicyMeta {
    /// The task's `allowed_tools` (empty = CLI defaults).
    pub allowed_tools: Vec<String>,
    /// Effective built-in tool switches handed to the SDK.
    pub builtin_tools: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecurityMeta {
    pub side_effects: serde_json::Value,
    pub approvals: String,
    /// OS sandbox backend for agent tools: `landlock` or `off` (#253).
    pub agent_sandbox: String,
    /// Mean per-call sandbox overhead measured for the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_sandbox_overhead_ms: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IsolationMeta {
    pub backend: String,
    pub network: bool,
    pub cpu_limit: Option<f64>,
    pub memory_limit: Option<u64>,
    pub container_image: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeMeta {
    pub max_turns: usize,
    pub timeout_secs: u64,
    pub mode: String,
    pub attempts: usize,
    pub repair_enabled: bool,
    pub repair_max_attempts: u32,
    pub repair_require_validation_after_mutation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptMeta {
    pub template_version: String,
    pub variant: Option<String>,
    /// `sha256:<hex>` of the initial prompt sent to the agent.
    pub hash: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    pub os: String,
    pub arch: String,
    pub cpus: usize,
    pub memory_bytes: Option<u64>,
}

impl HostInfo {
    pub fn gather() -> Self {
        HostInfo {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            cpus: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
            memory_bytes: total_memory_bytes(),
        }
    }
}

fn total_memory_bytes() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = text.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// Run-level context gathered once (git + host + build version).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEnvironment {
    pub git: GitMeta,
    pub host: HostInfo,
    pub crom_version: String,
}

impl RunEnvironment {
    pub fn gather() -> Self {
        let git = crate::index::GitContext::detect();
        RunEnvironment {
            git: GitMeta {
                sha: git.sha,
                dirty: git.dirty,
            },
            host: HostInfo::gather(),
            crom_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

/// `sha256:<hex>` of a byte string.
pub fn sha256_tag(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

/// Hash every regular file under `dir` (relative path + contents, sorted by
/// path). `None` when the directory is missing or unreadable.
pub fn task_content_hash(dir: &Path) -> Option<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, std::path::PathBuf)>) -> Option<()> {
        for entry in std::fs::read_dir(dir).ok()? {
            let entry = entry.ok()?;
            let path = entry.path();
            let ft = entry.file_type().ok()?;
            if ft.is_dir() {
                walk(root, &path, out)?;
            } else if ft.is_file() {
                let rel = path
                    .strip_prefix(root)
                    .ok()?
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, path));
            }
        }
        Some(())
    }
    if !dir.is_dir() {
        return None;
    }
    let mut files = Vec::new();
    walk(dir, dir, &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    for (rel, path) in files {
        let bytes = std::fs::read(&path).ok()?;
        hasher.update(rel.as_bytes());
        hasher.update([0u8]);
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Some(format!("sha256:{hex}"))
}

/// Inputs the runner already holds when it builds a result's repro block.
pub struct ReproInputs<'a> {
    pub task: &'a EvalTask,
    pub config: &'a RunConfig,
    pub env: &'a RunEnvironment,
    pub prompt: &'a str,
    pub model: String,
    pub base_url_host: Option<String>,
    pub builtin_tools: serde_json::Value,
    pub side_effects: serde_json::Value,
    pub approvals: &'a str,
}

impl ReproMetadata {
    pub fn build(i: ReproInputs<'_>) -> Self {
        let task = i.task;
        let config = i.config;
        ReproMetadata {
            git: i.env.git.clone(),
            task: TaskMeta {
                id: task.id.clone(),
                schema_version: task.schema_version,
                content_version: task.task_version.clone(),
                content_hash: task_content_hash(&task.dir),
            },
            provider: ProviderMeta {
                profile: config.provider.label(),
                model: i.model,
                base_url_host: i.base_url_host,
            },
            tool_policy: ToolPolicyMeta {
                allowed_tools: task.allowed_tools.clone(),
                builtin_tools: i.builtin_tools,
            },
            security: SecurityMeta {
                side_effects: i.side_effects,
                approvals: i.approvals.to_string(),
                agent_sandbox: match config.sandbox {
                    crom_core::SandboxMode::Required => "landlock",
                    crom_core::SandboxMode::Off => "off",
                }
                .to_string(),
                agent_sandbox_overhead_ms: config.sandbox_overhead_ms,
            },
            isolation: IsolationMeta {
                backend: config.isolation.id().to_string(),
                network: config.network || task.requires_network,
                cpu_limit: config.cpu_limit,
                memory_limit: config.memory_limit,
                container_image: config.container_image.clone(),
            },
            runtime: RuntimeMeta {
                max_turns: task.max_turns,
                timeout_secs: task.timeout.as_secs(),
                mode: serde_json::to_value(task.mode)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default(),
                attempts: config.attempts.max(1),
                repair_enabled: config.repair.enabled,
                repair_max_attempts: config.repair.max_attempts,
                repair_require_validation_after_mutation: config
                    .repair
                    .require_validation_after_mutation,
            },
            prompt: PromptMeta {
                template_version: PROMPT_TEMPLATE_VERSION.to_string(),
                variant: config.prompt_variant.clone(),
                hash: sha256_tag(i.prompt.as_bytes()),
            },
            repetition: config.repetition,
            seed: config.seed,
            host: i.env.host.clone(),
            crom_version: i.env.crom_version.clone(),
            pricing_source_version: None,
        }
    }
}

/// Factors that must match for two results to be compared (§8.3): task
/// content, tool policy, security config, isolation backend, runtime budgets,
/// and prompt. Provider/model/seed/host are deliberately excluded because
/// they are usually the subject of the experiment. Returns one
/// `"<factor>: <a> != <b>"` line per mismatch; empty means comparable.
pub fn comparability_diff(a: &ReproMetadata, b: &ReproMetadata) -> Vec<String> {
    fn cmp<T: Serialize>(name: &str, a: &T, b: &T, out: &mut Vec<String>) {
        let (va, vb) = (
            serde_json::to_value(a).unwrap_or_default(),
            serde_json::to_value(b).unwrap_or_default(),
        );
        if va != vb {
            out.push(format!("{name}: {va} != {vb}"));
        }
    }
    let mut out = Vec::new();
    cmp("task.id", &a.task.id, &b.task.id, &mut out);
    cmp(
        "task.content_version",
        &a.task.content_version,
        &b.task.content_version,
        &mut out,
    );
    cmp(
        "task.content_hash",
        &a.task.content_hash,
        &b.task.content_hash,
        &mut out,
    );
    cmp("tool_policy", &a.tool_policy, &b.tool_policy, &mut out);
    cmp("security", &a.security, &b.security, &mut out);
    cmp(
        "isolation.backend",
        &a.isolation.backend,
        &b.isolation.backend,
        &mut out,
    );
    cmp("runtime", &a.runtime, &b.runtime, &mut out);
    cmp(
        "prompt.template_version",
        &a.prompt.template_version,
        &b.prompt.template_version,
        &mut out,
    );
    cmp("prompt.hash", &a.prompt.hash, &b.prompt.hash, &mut out);
    out
}

/// Error from [`parse_result`].
#[derive(Debug, thiserror::Error)]
pub enum ResultParseError {
    #[error(
        "unsupported result schema_version {0} (this build reads 1..={RESULT_SCHEMA_VERSION})"
    )]
    UnsupportedVersion(u64),
    #[error("invalid result: {0}")]
    Invalid(#[from] serde_json::Error),
}

/// Parse a `result.json` value, accepting v1 (no `schema_version`) and v2.
/// v1 results load with `schema_version = 1`, `repro = None`, and an outcome
/// derived on demand by [`EvalResult::outcome`].
pub fn parse_result(value: serde_json::Value) -> Result<EvalResult, ResultParseError> {
    let version = value
        .get("schema_version")
        .and_then(|v| v.as_u64())
        .unwrap_or(LEGACY_RESULT_SCHEMA_VERSION as u64);
    if version == 0 || version > RESULT_SCHEMA_VERSION as u64 {
        return Err(ResultParseError::UnsupportedVersion(version));
    }
    Ok(serde_json::from_value(value)?)
}

/// Read and parse a `result.json` file (v1 or v2).
pub fn load_result(path: &Path) -> anyhow::Result<EvalResult> {
    let text = std::fs::read_to_string(path)?;
    Ok(parse_result(serde_json::from_str(&text)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Status, resolution, and failure class map onto the six outcome classes.
    #[test]
    fn outcome_derivation_covers_every_class() {
        use EvalStatus as S;
        use FailureClass as F;
        assert_eq!(Outcome::derive(S::Passed, true, None), Outcome::Resolved);
        assert_eq!(
            Outcome::derive(S::Failed, false, Some(F::PublicTestFailure)),
            Outcome::Failed
        );
        assert_eq!(
            Outcome::derive(S::TimedOut, false, Some(F::Timeout)),
            Outcome::Failed
        );
        assert_eq!(
            Outcome::derive(S::Errored, false, Some(F::ProviderError)),
            Outcome::ProviderError
        );
        for f in [F::HarnessError, F::IsolationError, F::AdapterError] {
            assert_eq!(
                Outcome::derive(S::Errored, false, Some(f)),
                Outcome::InfraError
            );
        }
        for f in [
            F::TaskInvalid,
            F::OracleValidationFailed,
            F::VerificationFlake,
        ] {
            assert_eq!(
                Outcome::derive(S::Failed, false, Some(f)),
                Outcome::Unscorable
            );
        }
        assert_eq!(Outcome::derive(S::Skipped, false, None), Outcome::Skipped);
    }

    /// Pass rate uses only resolved + failed; other classes are excluded from the denominator.
    #[test]
    fn pass_rate_denominator_is_resolved_plus_failed() {
        let c = OutcomeCounts::from_outcomes([
            Outcome::Resolved,
            Outcome::Failed,
            Outcome::Failed,
            Outcome::Skipped,
            Outcome::Unscorable,
        ]);
        assert_eq!(c.scored(), 3);
        assert_eq!(c.pass_rate(), Some(1.0 / 3.0));
        assert!(!c.unreliable());
        assert_eq!(OutcomeCounts::default().pass_rate(), None);
    }

    /// A run whose provider + infra errors exceed 10% of attempts is flagged unreliable.
    #[test]
    fn error_fraction_above_ten_percent_is_unreliable() {
        let mut outcomes = vec![Outcome::Resolved; 9];
        outcomes.push(Outcome::ProviderError);
        assert!(!OutcomeCounts::from_outcomes(outcomes.clone()).unreliable());
        outcomes.push(Outcome::InfraError);
        assert!(OutcomeCounts::from_outcomes(outcomes).unreliable());
    }

    /// The task content hash changes when any file in the task directory changes.
    #[test]
    fn content_hash_tracks_fixture_changes() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("task.toml"), "id = 'x'").unwrap();
        std::fs::create_dir(tmp.path().join("fixture")).unwrap();
        std::fs::write(tmp.path().join("fixture/a.rs"), "fn a() {}").unwrap();
        let h1 = task_content_hash(tmp.path()).unwrap();
        assert_eq!(task_content_hash(tmp.path()).unwrap(), h1);
        std::fs::write(tmp.path().join("fixture/a.rs"), "fn a() { 1; }").unwrap();
        assert_ne!(task_content_hash(tmp.path()).unwrap(), h1);
        assert!(task_content_hash(&tmp.path().join("missing")).is_none());
    }

    /// The checked-in JSON Schema parses and pins schema_version to the current constant.
    #[test]
    fn schema_json_pins_current_version() {
        let schema: serde_json::Value = serde_json::from_str(RESULT_SCHEMA_JSON).unwrap();
        assert_eq!(
            schema["properties"]["schema_version"]["const"],
            RESULT_SCHEMA_VERSION
        );
    }
}
