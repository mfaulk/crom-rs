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
    child: Child,
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

impl Deployment {
    /// Starts Guard and the business API; `ledger` is the API's effect log.
    pub async fn start(example_dir: &Path, bin: PathBuf, ledger: &Path) -> Result<Self> {
        let state = tempfile::Builder::new().prefix("guard-state-").tempdir()?;
        let dir = state.path();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        std::fs::copy(example_dir.join("guard.toml"), dir.join("guard.toml"))?;
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

        let agent_port = free_port()?;
        let api_port = free_port()?;
        let log = std::fs::File::create(dir.join("guard.log"))?;
        let child = Command::new(&bin)
            .arg("serve")
            .arg("--config")
            .arg(dir.join("guard.toml"))
            .arg("--agent-listen")
            .arg(format!("127.0.0.1:{agent_port}"))
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
            agent_url: format!("http://127.0.0.1:{agent_port}/mcp"),
            api_url: format!("http://127.0.0.1:{api_port}"),
            admin,
            admin_token,
            business_secret,
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
