//! OS sandbox for tool child processes (M17.3, #253; ADR 0001).
//!
//! Only Linux has a backend. Every other platform, and any Linux host that
//! cannot fully enforce the configuration, fails closed under
//! [`SandboxMode::Required`].

use std::path::Path;
use std::time::{Duration, Instant};

use crom_core::{SandboxConfig, SandboxMode};

#[cfg(target_os = "linux")]
mod linux;

/// What the backend reports when it can fully enforce the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxInfo {
    pub backend: &'static str,
    pub landlock_abi: u32,
}

/// Check whether the sandbox can fully enforce `cfg` on this host.
pub fn probe(cfg: &SandboxConfig) -> Result<SandboxInfo, String> {
    #[cfg(target_os = "linux")]
    {
        linux::probe(cfg)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = cfg;
        Err(format!(
            "no OS sandbox backend for {} (Linux only in M17)",
            std::env::consts::OS
        ))
    }
}

/// Parent-side handle for a sandboxed spawn; keep it alive until the child
/// has been reaped (it owns the private tmp dir).
pub(crate) struct Prepared {
    pub(crate) info: SandboxInfo,
    #[cfg(target_os = "linux")]
    _inner: linux::Prepared,
}

/// Configure `cmd` to run inside the sandbox rooted at `workspace`.
pub(crate) fn prepare(
    cfg: &SandboxConfig,
    workspace: &Path,
    cmd: &mut tokio::process::Command,
) -> Result<Prepared, String> {
    #[cfg(target_os = "linux")]
    {
        let inner = linux::prepare(cfg, workspace, cmd)?;
        Ok(Prepared {
            info: inner.info.clone(),
            _inner: inner,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (workspace, cmd);
        probe(cfg).map(|_| unreachable!("probe always fails without a backend"))
    }
}

/// Mean extra wall time (ms) a sandboxed `true` costs over an unsandboxed
/// one in `workspace`, from `reps` warm calls of each.
pub async fn measure_overhead_ms(
    cfg: &SandboxConfig,
    workspace: &Path,
    reps: u32,
) -> Result<f64, String> {
    use crate::{BashTool, ToolLimits};
    use crom_core::{SessionId, Tool, ToolContext};
    use std::sync::Arc;

    let reps = reps.max(1);
    let mut mean = [0f64; 2];
    for (i, mode) in [SandboxMode::Off, SandboxMode::Required]
        .into_iter()
        .enumerate()
    {
        let tool = BashTool::new(Arc::new(ToolLimits {
            bash_timeout: Duration::from_secs(10),
            sandbox: SandboxConfig {
                mode,
                ..cfg.clone()
            },
            ..ToolLimits::default()
        }));
        let call = || {
            tool.execute(
                serde_json::json!({"command": "true"}),
                ToolContext::new_minimal(workspace.to_path_buf(), SessionId::new()),
            )
        };
        call().await.map_err(|e| e.to_string())?;
        let start = Instant::now();
        for _ in 0..reps {
            call().await.map_err(|e| e.to_string())?;
        }
        mean[i] = start.elapsed().as_secs_f64() * 1000.0 / f64::from(reps);
    }
    Ok((mean[1] - mean[0]).max(0.0))
}
