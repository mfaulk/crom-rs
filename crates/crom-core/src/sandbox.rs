//! OS sandbox configuration for tool child processes (M17.3, #253).
//!
//! The enforcement backend lives in `crom-tools::sandbox`; this module only
//! holds the operator-facing settings so the CLI, SDK, and eval harness share
//! one schema. Repository settings may only tighten these values (see
//! [`SandboxSettingsConfig::tighten_from_untrusted`]).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Whether tool processes must run inside the OS sandbox.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    /// Run unsandboxed (legacy behavior). Tool results record `mode: off`.
    #[default]
    Off,
    /// Refuse to run a tool process unless the sandbox fully enforces.
    Required,
}

impl SandboxMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Required => "required",
        }
    }
}

impl std::str::FromStr for SandboxMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "required" => Ok(Self::Required),
            other => Err(format!(
                "invalid sandbox mode `{other}` (expected required|off)"
            )),
        }
    }
}

impl std::fmt::Display for SandboxMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Default per-call process budget (tasks above the UID's count at spawn).
pub const DEFAULT_SANDBOX_MAX_PROCESSES: u64 = 1024;
/// Default per-process data-segment limit (`RLIMIT_DATA`): 8 GiB.
pub const DEFAULT_SANDBOX_MAX_MEMORY_BYTES: u64 = 8 << 30;
/// Default per-file size limit (`RLIMIT_FSIZE`): 4 GiB.
pub const DEFAULT_SANDBOX_MAX_FILE_SIZE_BYTES: u64 = 4 << 30;
/// Default per-process CPU time limit (`RLIMIT_CPU`).
pub const DEFAULT_SANDBOX_MAX_CPU_SECONDS: u64 = 600;
/// Minimum landlock ABI accepted under `required` (ABI 3 restricts truncate).
pub const DEFAULT_SANDBOX_MIN_LANDLOCK_ABI: u32 = 3;

/// Effective sandbox configuration handed to tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxConfig {
    pub mode: SandboxMode,
    /// Allow IP networking (all-or-nothing; no host allowlist in M17).
    pub network: bool,
    /// Extra read-only paths on top of the built-in system/toolchain set.
    pub read_paths: Vec<PathBuf>,
    /// Extra environment variable names passed through to the child.
    pub env_allow: Vec<String>,
    pub max_processes: u64,
    pub max_memory_bytes: u64,
    pub max_file_size_bytes: u64,
    pub max_cpu_seconds: u64,
    pub min_landlock_abi: u32,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            mode: SandboxMode::Off,
            network: false,
            read_paths: Vec::new(),
            env_allow: Vec::new(),
            max_processes: DEFAULT_SANDBOX_MAX_PROCESSES,
            max_memory_bytes: DEFAULT_SANDBOX_MAX_MEMORY_BYTES,
            max_file_size_bytes: DEFAULT_SANDBOX_MAX_FILE_SIZE_BYTES,
            max_cpu_seconds: DEFAULT_SANDBOX_MAX_CPU_SECONDS,
            min_landlock_abi: DEFAULT_SANDBOX_MIN_LANDLOCK_ABI,
        }
    }
}

impl SandboxConfig {
    /// The default configuration with `mode = required`.
    pub fn required() -> Self {
        Self {
            mode: SandboxMode::Required,
            ..Self::default()
        }
    }
}

/// On-disk `sandbox` settings section. All fields optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxSettingsConfig {
    #[serde(default)]
    pub mode: Option<SandboxMode>,
    #[serde(default)]
    pub network: Option<bool>,
    #[serde(default)]
    pub read_paths: Option<Vec<PathBuf>>,
    #[serde(default)]
    pub env_allow: Option<Vec<String>>,
    #[serde(default)]
    pub max_processes: Option<u64>,
    #[serde(default)]
    pub max_memory_bytes: Option<u64>,
    #[serde(default)]
    pub max_file_size_bytes: Option<u64>,
    #[serde(default)]
    pub max_cpu_seconds: Option<u64>,
    #[serde(default)]
    pub min_landlock_abi: Option<u32>,
}

fn min_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

impl SandboxSettingsConfig {
    /// Operator-sourced overlay: scalars overwrite, lists concatenate.
    pub fn merge(&mut self, src: SandboxSettingsConfig) {
        if src.mode.is_some() {
            self.mode = src.mode;
        }
        if src.network.is_some() {
            self.network = src.network;
        }
        if let Some(p) = src.read_paths {
            self.read_paths.get_or_insert_with(Vec::new).extend(p);
        }
        if let Some(e) = src.env_allow {
            self.env_allow.get_or_insert_with(Vec::new).extend(e);
        }
        for (dst, v) in [
            (&mut self.max_processes, src.max_processes),
            (&mut self.max_memory_bytes, src.max_memory_bytes),
            (&mut self.max_file_size_bytes, src.max_file_size_bytes),
            (&mut self.max_cpu_seconds, src.max_cpu_seconds),
        ] {
            if v.is_some() {
                *dst = v;
            }
        }
        if src.min_landlock_abi.is_some() {
            self.min_landlock_abi = src.min_landlock_abi;
        }
    }

    /// Repository-sourced overlay (`<cwd>/.agent/settings.json`): may only
    /// tighten. `mode=required`, `network=false`, lower limits, and a higher
    /// minimum ABI apply; `mode=off`, `network=true`, extra read paths, and
    /// extra env names are ignored.
    pub fn tighten_from_untrusted(&mut self, src: SandboxSettingsConfig) {
        if src.mode == Some(SandboxMode::Required) {
            self.mode = Some(SandboxMode::Required);
        }
        if src.network == Some(false) {
            self.network = Some(false);
        }
        self.max_processes = min_opt(self.max_processes, src.max_processes);
        self.max_memory_bytes = min_opt(self.max_memory_bytes, src.max_memory_bytes);
        self.max_file_size_bytes = min_opt(self.max_file_size_bytes, src.max_file_size_bytes);
        self.max_cpu_seconds = min_opt(self.max_cpu_seconds, src.max_cpu_seconds);
        if let Some(abi) = src.min_landlock_abi {
            self.min_landlock_abi = Some(self.min_landlock_abi.unwrap_or(0).max(abi));
        }
    }

    /// Resolve against the defaults.
    pub fn resolve(&self) -> SandboxConfig {
        let d = SandboxConfig::default();
        SandboxConfig {
            mode: self.mode.unwrap_or(d.mode),
            network: self.network.unwrap_or(d.network),
            read_paths: self.read_paths.clone().unwrap_or_default(),
            env_allow: self.env_allow.clone().unwrap_or_default(),
            max_processes: self.max_processes.unwrap_or(d.max_processes),
            max_memory_bytes: self.max_memory_bytes.unwrap_or(d.max_memory_bytes),
            max_file_size_bytes: self.max_file_size_bytes.unwrap_or(d.max_file_size_bytes),
            max_cpu_seconds: self.max_cpu_seconds.unwrap_or(d.max_cpu_seconds),
            min_landlock_abi: self
                .min_landlock_abi
                .unwrap_or(d.min_landlock_abi)
                .max(d.min_landlock_abi),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Repository settings cannot turn the sandbox off, grant network, or add read paths/env names.
    #[test]
    fn untrusted_overlay_cannot_loosen() {
        let mut s = SandboxSettingsConfig {
            mode: Some(SandboxMode::Required),
            network: Some(false),
            max_processes: Some(100),
            ..Default::default()
        };
        s.tighten_from_untrusted(SandboxSettingsConfig {
            mode: Some(SandboxMode::Off),
            network: Some(true),
            read_paths: Some(vec!["/home".into()]),
            env_allow: Some(vec!["ANTHROPIC_API_KEY".into()]),
            max_processes: Some(10_000),
            min_landlock_abi: Some(1),
            ..Default::default()
        });
        let r = s.resolve();
        assert_eq!(r.mode, SandboxMode::Required);
        assert!(!r.network);
        assert!(r.read_paths.is_empty());
        assert!(r.env_allow.is_empty());
        assert_eq!(r.max_processes, 100);
        assert_eq!(r.min_landlock_abi, DEFAULT_SANDBOX_MIN_LANDLOCK_ABI);
    }

    // Repository settings can tighten: require the sandbox and lower limits.
    #[test]
    fn untrusted_overlay_can_tighten() {
        let mut s = SandboxSettingsConfig::default();
        s.tighten_from_untrusted(SandboxSettingsConfig {
            mode: Some(SandboxMode::Required),
            max_memory_bytes: Some(1 << 20),
            min_landlock_abi: Some(6),
            ..Default::default()
        });
        let r = s.resolve();
        assert_eq!(r.mode, SandboxMode::Required);
        assert_eq!(r.max_memory_bytes, 1 << 20);
        assert_eq!(r.min_landlock_abi, 6);
    }

    // The minimum landlock ABI never resolves below the built-in floor of 3.
    #[test]
    fn min_abi_has_floor() {
        let s = SandboxSettingsConfig {
            min_landlock_abi: Some(1),
            ..Default::default()
        };
        assert_eq!(s.resolve().min_landlock_abi, 3);
    }

    // Mode strings parse case-insensitively and reject unknown values.
    #[test]
    fn mode_parses() {
        assert_eq!("Required".parse::<SandboxMode>(), Ok(SandboxMode::Required));
        assert_eq!("off".parse::<SandboxMode>(), Ok(SandboxMode::Off));
        assert!("auto".parse::<SandboxMode>().is_err());
    }
}
