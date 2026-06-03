//! Isolation backends (M15B).
//!
//! Verification commands run through an [`IsolationBackend`]. The `local`
//! backend (the default) runs commands directly in the workspace directory and
//! works everywhere. The `docker`/`podman` backends run commands inside a
//! container and are best-effort: if the engine is unavailable they report an
//! [`IsolationError`] and the caller decides whether to skip.
//!
//! Network is disabled by default for verification where the backend can
//! enforce it (containers via `--network none`). The local backend cannot
//! enforce network isolation, so it records the *intent* in [`EnvMetadata`]
//! rather than overclaiming enforcement.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::oracle::{run_command, CommandResult};

/// Opt-in env var for the Docker isolation lane.
pub const DOCKER_ENV: &str = "CODE_AGENT_EVAL_DOCKER";
/// Opt-in env var for the Podman isolation lane.
pub const PODMAN_ENV: &str = "CODE_AGENT_EVAL_PODMAN";

/// Which isolation backend to use.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum IsolationKind {
    #[default]
    Local,
    Docker,
    Podman,
}

impl IsolationKind {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "local" => Ok(Self::Local),
            "docker" => Ok(Self::Docker),
            "podman" => Ok(Self::Podman),
            other => Err(format!(
                "unknown isolation backend '{other}'; expected local, docker, or podman"
            )),
        }
    }

    pub fn id(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Docker => "docker",
            Self::Podman => "podman",
        }
    }
}

/// Resource and network limits applied to a verification command.
#[derive(Debug, Clone)]
pub struct CommandLimits {
    pub timeout: Duration,
    /// Whether network access is permitted. Default false for verification.
    pub network: bool,
    /// Optional CPU limit (cores). Enforced by container backends only.
    pub cpu_limit: Option<f64>,
    /// Optional memory limit in bytes. Enforced by container backends only.
    pub memory_limit: Option<u64>,
    /// Container image (container backends only).
    pub container_image: Option<String>,
}

impl Default for CommandLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(180),
            network: false,
            cpu_limit: None,
            memory_limit: None,
            container_image: None,
        }
    }
}

/// Outcome of running a single command through a backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandOutcome {
    #[serde(flatten)]
    pub result: CommandResult,
    /// Backend that executed the command.
    pub backend: String,
}

/// An isolation backend error (engine missing, container failed, etc.).
#[derive(Debug, Clone, thiserror::Error)]
#[error("isolation error ({backend}): {message}")]
pub struct IsolationError {
    pub backend: String,
    pub message: String,
}

/// Environment metadata recorded in `environment.json` for every run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvMetadata {
    pub os: String,
    pub arch: String,
    pub rustc: Option<String>,
    pub cargo: Option<String>,
    pub isolation: String,
    pub container_engine: Option<String>,
    pub container_image: Option<String>,
    pub network: bool,
}

impl EnvMetadata {
    /// Gather host + isolation metadata. Tool versions are best-effort.
    pub fn gather(backend: &dyn IsolationBackend, network: bool) -> Self {
        EnvMetadata {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            rustc: tool_version("rustc"),
            cargo: tool_version("cargo"),
            isolation: backend.id().to_string(),
            container_engine: backend.engine_version(),
            container_image: backend.container_image(),
            network,
        }
    }
}

/// Best-effort `--version` capture for a CLI tool.
fn tool_version(tool: &str) -> Option<String> {
    let out = std::process::Command::new(tool)
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    Some(s.lines().next().unwrap_or("").trim().to_string())
}

/// An isolation backend executes verification commands in some isolated
/// context and reports environment metadata.
#[async_trait]
pub trait IsolationBackend: Send + Sync {
    /// Stable backend id (`local` / `docker` / `podman`).
    fn id(&self) -> &'static str;

    /// Container engine version string, if this backend uses one.
    fn engine_version(&self) -> Option<String> {
        None
    }

    /// Container image, if this backend uses one.
    fn container_image(&self) -> Option<String> {
        None
    }

    /// Run a single command in `workspace` under `limits`.
    async fn run(
        &self,
        workspace: &Path,
        command: &str,
        limits: &CommandLimits,
    ) -> Result<CommandOutcome, IsolationError>;
}

/// The default backend: runs commands directly in the workspace directory.
#[derive(Debug, Default, Clone)]
pub struct LocalBackend;

#[async_trait]
impl IsolationBackend for LocalBackend {
    fn id(&self) -> &'static str {
        "local"
    }

    async fn run(
        &self,
        workspace: &Path,
        command: &str,
        limits: &CommandLimits,
    ) -> Result<CommandOutcome, IsolationError> {
        let result = run_command(command, workspace, limits.timeout).await;
        Ok(CommandOutcome {
            result,
            backend: "local".to_string(),
        })
    }
}

/// A container backend (`docker` or `podman`). Best-effort: detects the engine
/// at construction and runs commands via `engine run`.
#[derive(Debug, Clone)]
pub struct ContainerBackend {
    engine: String,
    version: Option<String>,
    image: String,
}

/// Default image used when none is supplied. Chosen to have a Rust toolchain.
pub const DEFAULT_CONTAINER_IMAGE: &str = "rust:latest";

impl ContainerBackend {
    /// Create a container backend, verifying the engine is available. The
    /// opt-in env var must be set, mirroring the live-provider gating.
    pub fn new(engine: &str, image: Option<String>) -> Result<Self, IsolationError> {
        let opt_in = match engine {
            "docker" => DOCKER_ENV,
            "podman" => PODMAN_ENV,
            _ => "",
        };
        if !opt_in.is_empty() && std::env::var(opt_in).ok().as_deref() != Some("1") {
            return Err(IsolationError {
                backend: engine.to_string(),
                message: format!("{engine} isolation requires {opt_in}=1 (opt-in)"),
            });
        }
        let version = tool_version(engine);
        if version.is_none() {
            return Err(IsolationError {
                backend: engine.to_string(),
                message: format!("{engine} engine not found on PATH"),
            });
        }
        Ok(Self {
            engine: engine.to_string(),
            version,
            image: image.unwrap_or_else(|| DEFAULT_CONTAINER_IMAGE.to_string()),
        })
    }
}

#[async_trait]
impl IsolationBackend for ContainerBackend {
    fn id(&self) -> &'static str {
        // We cannot return a dynamic &'static str; map known engines.
        if self.engine == "podman" {
            "podman"
        } else {
            "docker"
        }
    }

    fn engine_version(&self) -> Option<String> {
        self.version.clone()
    }

    fn container_image(&self) -> Option<String> {
        Some(self.image.clone())
    }

    async fn run(
        &self,
        workspace: &Path,
        command: &str,
        limits: &CommandLimits,
    ) -> Result<CommandOutcome, IsolationError> {
        let ws = workspace.to_string_lossy().to_string();
        let mut args: Vec<String> = vec![
            "run".into(),
            "--rm".into(),
            "-v".into(),
            format!("{ws}:/work"),
            "-w".into(),
            "/work".into(),
        ];
        if !limits.network {
            args.push("--network".into());
            args.push("none".into());
        }
        if let Some(cpus) = limits.cpu_limit {
            args.push("--cpus".into());
            args.push(format!("{cpus}"));
        }
        if let Some(mem) = limits.memory_limit {
            args.push("--memory".into());
            args.push(format!("{mem}"));
        }
        let image = limits
            .container_image
            .clone()
            .unwrap_or_else(|| self.image.clone());
        args.push(image);
        // The verification command is passed as a single `sh -c` argument so the
        // shell *inside* the container splits it — not the host. Running the
        // engine via an explicit argv (rather than a joined host `sh -c`) keeps
        // multi-word commands like `cargo test --all-targets` intact.
        args.push("sh".into());
        args.push("-c".into());
        args.push(command.to_string());

        let result =
            crate::oracle::run_argv(&self.engine, &args, Path::new("."), limits.timeout).await;
        Ok(CommandOutcome {
            result,
            backend: self.engine.clone(),
        })
    }
}

/// Construct a backend for an [`IsolationKind`]. Container backends may fail if
/// the engine is unavailable or opt-in is missing.
pub fn build_backend(
    kind: &IsolationKind,
    image: Option<String>,
) -> Result<Box<dyn IsolationBackend>, IsolationError> {
    match kind {
        IsolationKind::Local => Ok(Box::new(LocalBackend)),
        IsolationKind::Docker => Ok(Box::new(ContainerBackend::new("docker", image)?)),
        IsolationKind::Podman => Ok(Box::new(ContainerBackend::new("podman", image)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kinds() {
        assert_eq!(IsolationKind::parse("local").unwrap(), IsolationKind::Local);
        assert_eq!(
            IsolationKind::parse("docker").unwrap(),
            IsolationKind::Docker
        );
        assert_eq!(
            IsolationKind::parse("podman").unwrap(),
            IsolationKind::Podman
        );
        assert!(IsolationKind::parse("nope").is_err());
    }

    #[tokio::test]
    async fn local_backend_runs_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = LocalBackend;
        let limits = CommandLimits {
            timeout: Duration::from_secs(10),
            ..Default::default()
        };
        let ok = backend.run(tmp.path(), "exit 0", &limits).await.unwrap();
        assert!(ok.result.success);
        assert_eq!(ok.backend, "local");
        let bad = backend.run(tmp.path(), "exit 7", &limits).await.unwrap();
        assert!(!bad.result.success);
        assert_eq!(bad.result.exit_code, Some(7));
    }

    #[tokio::test]
    async fn local_backend_enforces_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = LocalBackend;
        let limits = CommandLimits {
            timeout: Duration::from_millis(150),
            ..Default::default()
        };
        let res = backend.run(tmp.path(), "sleep 5", &limits).await.unwrap();
        assert!(res.result.timed_out);
    }

    #[test]
    fn env_metadata_records_isolation() {
        let backend = LocalBackend;
        let meta = EnvMetadata::gather(&backend, false);
        assert_eq!(meta.isolation, "local");
        assert!(meta.container_engine.is_none());
        assert!(!meta.network);
        assert!(!meta.os.is_empty());
    }

    #[test]
    fn docker_requires_opt_in_or_engine() {
        // In ordinary CI, neither opt-in nor engine is present; constructing a
        // container backend must error rather than silently succeed.
        if std::env::var(DOCKER_ENV).ok().as_deref() != Some("1") {
            assert!(ContainerBackend::new("docker", None).is_err());
        }
    }
}
