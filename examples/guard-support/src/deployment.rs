//! Starts the reference Guard (`crom-guard-support`, docs/guard/reference-deployment.md)
//! in its own process, from a private state directory, as run-python-client.sh does.

use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::process::{Child, Command};

use crate::admin::{Admin, Secret};

pub struct Deployment {
    pub bin: PathBuf,
    /// Guard's 0700 state directory: config, policies, admin token,
    /// credentials, admin socket, audit log, Guard's own log.
    pub state: tempfile::TempDir,
    pub agent_url: String,
    pub api_url: String,
    pub admin: Arc<Admin>,
    pub admin_token: Secret,
    pub business_secret: Secret,
    /// `crom-guard check` passed before Guard started; its `ok:` count.
    pub preflight: String,
    /// Crom's config directory as `check` saw it (holds `settings.json`).
    _crom_config: tempfile::TempDir,
    child: Child,
}

/// The Crom agent the deployment is checked for: an in-process SDK agent on
/// Guard's uid, sandboxed (agent (a) in docs/guard/deployment.md).
pub struct CromAgent<'a> {
    pub workspace: &'a Path,
    pub bash: bool,
}

/// `guard.toml` with the agent endpoint on `port`, so the file `check`
/// reads is the file `serve` runs with.
pub fn config_with_port(config: &str, port: u16) -> Result<String> {
    const DEFAULT: &str = "listen = \"127.0.0.1:8470\"";
    if config.matches(DEFAULT).count() != 1 {
        bail!("guard.toml: expected one `{DEFAULT}`");
    }
    Ok(config.replace(DEFAULT, &format!("listen = \"127.0.0.1:{port}\"")))
}

/// `CROM_GUARD_SUPPORT_BIN`, or the testkit example built from the repo.
pub fn guard_binary(root: &Path) -> Result<PathBuf> {
    if let Some(bin) = std::env::var_os("CROM_GUARD_SUPPORT_BIN") {
        return Ok(PathBuf::from(bin));
    }
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let manifest = root.join("Cargo.toml");
    eprintln!("building crom-guard-support ...");
    let status = std::process::Command::new(&cargo)
        .args(["build", "--locked", "-q", "-p", "crom-guard-testkit"])
        .args(["--example", "crom-guard-support", "--manifest-path"])
        .arg(&manifest)
        .status()
        .context("running cargo build")?;
    if !status.success() {
        bail!("cargo build of crom-guard-support failed");
    }
    let meta = std::process::Command::new(&cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--manifest-path",
        ])
        .arg(&manifest)
        .output()
        .context("running cargo metadata")?;
    let meta: serde_json::Value = serde_json::from_slice(&meta.stdout)?;
    let target = meta["target_directory"]
        .as_str()
        .context("cargo metadata: no target_directory")?;
    Ok(Path::new(target).join("debug/examples/crom-guard-support"))
}

fn random_hex(bytes: usize) -> Result<String> {
    use std::io::Read;
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(contents.as_bytes())?;
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

fn free_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port())
}

/// `crom-guard check --crom-settings --crom-workspace` (#360) against the
/// state directory `dir`, with the agent's policy rendered as Crom's user
/// settings in a fresh `CROM_CONFIG_HOME`.
fn preflight(
    bin: &Path,
    dir: &Path,
    agent_url: &str,
    out: &Path,
    agent: CromAgent<'_>,
) -> Result<(tempfile::TempDir, String)> {
    let crom_config = tempfile::Builder::new().prefix("crom-config-").tempdir()?;
    std::fs::set_permissions(crom_config.path(), std::fs::Permissions::from_mode(0o700))?;
    let settings_path = crom_config.path().join("settings.json");
    let settings = crate::agent::crom_settings(agent_url, agent.bash)?;
    write_private(&settings_path, &serde_json::to_string_pretty(&settings)?)?;
    std::fs::write(out.join("crom-settings.checked.json"), settings.to_string())?;
    let result = std::process::Command::new(bin)
        .arg("check")
        .arg("--config")
        .arg(dir.join("guard.toml"))
        .arg("--crom-settings")
        .arg(&settings_path)
        .arg("--crom-workspace")
        .arg(agent.workspace)
        .env("CROM_CONFIG_HOME", crom_config.path())
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {} check", bin.display()))?;
    let mut text = String::from_utf8_lossy(&result.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&result.stderr));
    std::fs::write(out.join("guard-check.txt"), &text)?;
    if !result.status.success() {
        bail!(
            "crom-guard check failed ({}); not starting Guard:\n{text}",
            result.status
        );
    }
    let ok = text.lines().filter(|l| l.starts_with("ok: ")).count();
    if ok == 0 || text.lines().any(|l| l.starts_with("FAIL: ")) {
        bail!("crom-guard check printed no passing check:\n{text}");
    }
    Ok((crom_config, format!("{ok} checks ok (guard-check.txt)")))
}

impl Deployment {
    /// Runs `crom-guard check` for `agent`, then starts Guard and the
    /// business API; `ledger` is the API's effect log. Fails if the check
    /// fails; its output is written to `out/guard-check.txt`.
    pub async fn start(
        example_dir: &Path,
        bin: PathBuf,
        ledger: &Path,
        out: &Path,
        agent: CromAgent<'_>,
    ) -> Result<Self> {
        let state = tempfile::Builder::new().prefix("guard-state-").tempdir()?;
        let dir = state.path();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        let agent_port = free_port()?;
        let api_port = free_port()?;
        let config = std::fs::read_to_string(example_dir.join("guard.toml"))?;
        std::fs::write(
            dir.join("guard.toml"),
            config_with_port(&config, agent_port)?,
        )?;
        copy_dir(&example_dir.join("policies"), &dir.join("policies"))?;
        let admin_token = Secret::new(format!("adm-{}", random_hex(32)?));
        let business_secret = Secret::new(format!("sk-business-{}", random_hex(16)?));
        write_private(&dir.join("admin.token"), admin_token.expose())?;
        let creds = serde_json::json!({"credentials": [{
            "id": "business-api",
            "secret": business_secret.expose(),
            "actions": ["customer.read", "invoice.list", "refund.issue", "customer.export"],
        }]});
        write_private(&dir.join("credentials.json"), &creds.to_string())?;

        let agent_url = format!("http://127.0.0.1:{agent_port}/mcp");
        let (crom_config, preflight) = preflight(&bin, dir, &agent_url, out, agent)?;

        let log = std::fs::File::create(dir.join("guard.log"))?;
        let child = Command::new(&bin)
            .arg("serve")
            .arg("--config")
            .arg(dir.join("guard.toml"))
            .env(
                "CROM_GUARD_SUPPORT_CREDENTIALS",
                dir.join("credentials.json"),
            )
            .env(
                "CROM_GUARD_SUPPORT_API_LISTEN",
                format!("127.0.0.1:{api_port}"),
            )
            .env("CROM_GUARD_SUPPORT_LEDGER", ledger)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", bin.display()))?;
        let admin = Arc::new(Admin::new(dir.join("admin.sock"), admin_token.clone()));
        let mut d = Self {
            bin,
            state,
            agent_url,
            api_url: format!("http://127.0.0.1:{api_port}"),
            admin,
            admin_token,
            business_secret,
            preflight,
            _crom_config: crom_config,
            child,
        };
        for _ in 0..200 {
            if d.admin.health().await.is_ok() {
                return Ok(d);
            }
            if let Some(status) = d.child.try_wait()? {
                let log = std::fs::read_to_string(d.guard_log()).unwrap_or_default();
                bail!("Guard exited ({status}) before it was ready:\n{log}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!("Guard did not become ready")
    }

    pub fn audit_path(&self) -> PathBuf {
        self.state.path().join("audit.jsonl")
    }

    pub fn guard_log(&self) -> PathBuf {
        self.state.path().join("guard.log")
    }

    /// `crom-guard audit verify` on Guard's audit log.
    pub fn verify_audit(&self) -> Result<bool> {
        let out = std::process::Command::new(&self.bin)
            .args(["audit", "verify"])
            .arg(self.audit_path())
            .output()?;
        Ok(out.status.success())
    }

    /// SIGTERM, then wait, so the audit log is complete.
    pub async fn stop(&mut self) -> Result<()> {
        if let Some(pid) = self.child.id() {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }
        match tokio::time::timeout(Duration::from_secs(10), self.child.wait()).await {
            Ok(r) => r.map(drop).map_err(Into::into),
            Err(_) => {
                self.child.start_kill()?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The agent port is written into the one `[agent] listen` line, so `check` and `serve` read the same endpoint.
    #[test]
    fn config_with_port_sets_the_checked_agent_port() {
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("guard.toml");
        let config = std::fs::read_to_string(example).unwrap();
        let out = config_with_port(&config, 41234).unwrap();
        assert!(out.contains("listen = \"127.0.0.1:41234\""));
        assert!(!out.contains("8470"));
        assert!(out.contains("listen = \"unix:admin.sock\""));
        assert!(config_with_port("[agent]\n", 1).is_err());
    }

    fn fake_check(dir: &Path, script: &str) -> PathBuf {
        let bin = dir.join("fake-guard");
        std::fs::write(&bin, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    /// A failing `crom-guard check` stops the start with its output kept in guard-check.txt; a passing one is summarized.
    #[test]
    fn preflight_failure_refuses_to_start() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let agent = || CromAgent {
            workspace: ws.path(),
            bash: false,
        };
        let url = "http://127.0.0.1:1/mcp";
        let bin = fake_check(tmp.path(), "echo 'FAIL: sandbox-mode: off'; exit 1");
        let err = preflight(&bin, tmp.path(), url, tmp.path(), agent()).unwrap_err();
        assert!(err.to_string().contains("not starting Guard"), "{err}");
        let saved = std::fs::read_to_string(tmp.path().join("guard-check.txt")).unwrap();
        assert!(saved.contains("FAIL: sandbox-mode"));

        let bin = fake_check(tmp.path(), "echo 'ok: config: x'; echo 'ok: guard-user: y'");
        let (_, summary) = preflight(&bin, tmp.path(), url, tmp.path(), agent()).unwrap();
        assert_eq!(summary, "2 checks ok (guard-check.txt)");
    }
}
