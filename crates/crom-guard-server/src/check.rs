//! `crom-guard check`: the isolation-profile preflight
//! (`docs/guard/deployment.md`). It reads files and metadata only: it binds
//! nothing, runs no agent, and never prints a secret. Crom's settings, trust
//! store and sandbox read set are modelled here rather than imported, so this
//! crate stays free of other `crom-*` crates; `crom-guard-testkit` pins the
//! copies against Crom's own code.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::{AdminListen, Config};

/// What to check. Paths are as given on the command line.
#[derive(Debug, Clone, Default)]
pub struct CheckOptions {
    pub config: PathBuf,
    /// Crom's user settings file, for an agent sandboxed by Crom on Guard's uid.
    pub crom_settings: Option<PathBuf>,
    /// An MCP config file Crom is started with (`--mcp-config`).
    pub crom_mcp_config: Option<PathBuf>,
    /// The directory Crom's agent works in (read-write inside the sandbox).
    /// Required with `crom_settings`.
    pub crom_workspace: Option<PathBuf>,
    /// Uid of an agent running under its own user.
    pub agent_uid: Option<u32>,
    /// Crom's config directory. `None`: resolved from the environment as
    /// Crom does ([`crom_config_dir`]).
    pub crom_config_dir: Option<PathBuf>,
    /// The sandbox's built-in read set. `None`: computed from the
    /// environment as Crom does ([`sandbox_builtin_read_paths`]).
    pub sandbox_read_paths: Option<Vec<PathBuf>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Stable identifier, e.g. `credentials` or `sandbox-network`.
    pub check: &'static str,
    pub passed: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub findings: Vec<Finding>,
}

impl Report {
    pub fn passed(&self) -> bool {
        self.findings.iter().all(|f| f.passed)
    }

    /// Ids of failed checks, in order, without repeats.
    pub fn failed(&self) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::new();
        for f in self.findings.iter().filter(|f| !f.passed) {
            if !out.contains(&f.check) {
                out.push(f.check);
            }
        }
        out
    }

    fn ok(&mut self, check: &'static str, detail: impl Into<String>) {
        self.push(check, true, detail);
    }

    fn fail(&mut self, check: &'static str, detail: impl Into<String>) {
        self.push(check, false, detail);
    }

    /// One finding: ok with `ok_detail` when `problems` is empty, else a
    /// failure listing them.
    fn all(&mut self, check: &'static str, problems: Vec<String>, ok_detail: &str) {
        if problems.is_empty() {
            self.ok(check, ok_detail);
        } else {
            self.fail(check, problems.join("; "));
        }
    }

    fn push(&mut self, check: &'static str, passed: bool, detail: impl Into<String>) {
        self.findings.push(Finding {
            check,
            passed,
            detail: detail.into(),
        });
    }
}

/// Resolves `--agent-user`: a numeric uid or a name in `/etc/passwd`.
pub fn resolve_user(user: &str) -> Result<u32, String> {
    if let Ok(uid) = user.parse() {
        return Ok(uid);
    }
    passwd_field(|name, _| name == user, 2)
        .and_then(|uid| uid.parse().ok())
        .ok_or_else(|| format!("no user {user:?} in /etc/passwd"))
}

/// Field `n` of the first `/etc/passwd` entry matching `(name, uid)`.
fn passwd_field(matches: impl Fn(&str, &str) -> bool, n: usize) -> Option<String> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() > n && matches(f[0], f[2])).then(|| f[n].to_owned())
    })
}

/// The sandbox's built-in read set, mirroring
/// `crom_tools::sandbox::builtin_read_paths` (pinned by a test there).
pub fn sandbox_builtin_read_paths() -> Vec<PathBuf> {
    builtin_read_paths_with(|k| std::env::var_os(k))
}

const BUILTIN_READ_ROOTS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/etc",
    "/proc",
    "/sys/devices/system/cpu",
    "/dev/zero",
    "/dev/random",
    "/dev/urandom",
    "/nix/store",
];

fn builtin_read_paths_with(var: impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let home = var("HOME").map(PathBuf::from);
    let env_path = |name: &str, rel: &str| {
        var(name)
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(rel)))
            .filter(|p| p.exists())
    };
    let mut ro: Vec<PathBuf> = BUILTIN_READ_ROOTS.iter().map(PathBuf::from).collect();
    if let Some(ch) = env_path("CARGO_HOME", ".cargo") {
        for sub in ["bin", "registry", "git", "config.toml", "config"] {
            ro.push(ch.join(sub));
        }
    }
    ro.extend(env_path("RUSTUP_HOME", ".rustup"));
    if let Some(path) = var("PATH") {
        for name in ["cargo", "rustc", "rustup"] {
            if let Some(bin) = std::env::split_paths(&path)
                .map(|d| d.join(name))
                .find(|p| p.is_file())
            {
                ro.extend(bin.parent().map(Path::to_path_buf));
                if let Some(dir) = bin
                    .canonicalize()
                    .ok()
                    .and_then(|c| c.parent().map(Path::to_path_buf))
                {
                    ro.push(dir);
                }
            }
        }
    }
    ro
}

/// Crom's user config directory, resolved as `crom_core::settings::user_settings_path`
/// does: `$CROM_CONFIG_HOME` (or legacy `$CODE_AGENT_CONFIG_HOME`), else
/// `${XDG_CONFIG_HOME:-$HOME/.config}/crom` (or the legacy `code-agent`
/// directory when only that exists).
pub fn crom_config_dir() -> Option<PathBuf> {
    let env = |k: &str| std::env::var(k).ok();
    if let Some(p) = env("CROM_CONFIG_HOME").or_else(|| env("CODE_AGENT_CONFIG_HOME")) {
        return Some(PathBuf::from(p));
    }
    let parent = env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    let (new, old) = (parent.join("crom"), parent.join("code-agent"));
    Some(if !new.exists() && old.is_dir() {
        old
    } else {
        new
    })
}

/// Crom's trust store (`crom_core::trust::trust_store_path`; no legacy
/// directory fallback).
fn crom_trust_store() -> Option<PathBuf> {
    let env = |k: &str| std::env::var(k).ok();
    if let Some(p) = env("CROM_CONFIG_HOME").or_else(|| env("CODE_AGENT_CONFIG_HOME")) {
        return Some(PathBuf::from(p).join(TRUST_STORE_FILE));
    }
    let parent = env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(parent.join("crom").join(TRUST_STORE_FILE))
}

const TRUST_STORE_FILE: &str = "trusted-workspaces.json";

/// `p` made absolute against the current directory, with the longest
/// existing prefix resolved through symlinks.
fn canon(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    let mut existing = abs.as_path();
    let mut rest = Vec::new();
    loop {
        if let Ok(c) = existing.canonicalize() {
            let mut out = c;
            out.extend(rest.iter().rev());
            return out;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => return abs,
        }
    }
}

#[cfg(not(unix))]
pub fn run(_opts: &CheckOptions) -> Report {
    let mut r = Report::default();
    r.fail(
        "platform",
        "the isolation profile is defined for Linux only; nothing was checked",
    );
    r
}

#[cfg(unix)]
pub fn run(opts: &CheckOptions) -> Report {
    unix::run(opts, rustix::process::geteuid().as_raw())
}

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    use super::*;

    pub(super) fn run(opts: &CheckOptions, euid: u32) -> Report {
        let mut r = Report::default();
        if euid == 0 {
            r.fail(
                "guard-user",
                "Guard runs as root; run it as its own unprivileged user",
            );
        } else {
            r.ok("guard-user", format!("Guard runs as uid {euid}"));
        }
        let config = match Config::load(&opts.config).and_then(|c| c.validate().map(|()| c)) {
            Ok(c) => {
                r.ok(
                    "config",
                    format!("{} loads and validates", opts.config.display()),
                );
                c
            }
            Err(e) => {
                r.fail("config", e.to_string());
                return r;
            }
        };
        if config.agent.listen.port() == 0 {
            r.fail(
                "agent-listen",
                "agent.listen has port 0: an agent cannot be pointed at a fixed Guard endpoint",
            );
        } else {
            r.ok(
                "agent-listen",
                format!("agent endpoint {}", config.agent.listen),
            );
        }
        r.all(
            "config-file",
            unsafe_components(&opts.config, euid),
            "guard.toml and its directories are owned by Guard or root and not writable by others",
        );
        let mut policy = unsafe_components(&config.policy.schema, euid);
        policy.extend(unsafe_components(&config.policy.policies, euid));
        r.all(
            "policy-files",
            policy,
            "schema, policies and their directories are owned by Guard or root and not \
             writable by others",
        );
        let private = private_paths(&config);
        let mut state = Vec::new();
        for p in &private {
            state.extend(unsafe_components(p, euid));
        }
        state.dedup();
        r.all(
            "state-path",
            state,
            "every component of Guard's state paths is owned by Guard or root and not \
             writable by others",
        );
        guard_files(&mut r, &config, euid);
        private_dirs(&mut r, &private, euid);
        admin_listener(&mut r, &config, opts, euid);

        if opts.crom_settings.is_none() && opts.agent_uid.is_none() {
            r.fail(
                "agent-isolation",
                "no agent isolation declared: pass --crom-settings for a Crom agent \
                 sandboxed with sandbox.mode = \"required\", or --agent-user for an agent \
                 under its own uid",
            );
        }
        if let Some(uid) = opts.agent_uid {
            agent_uid(&mut r, uid, euid);
        }
        if let Some(path) = &opts.crom_settings {
            super::crom::check(&mut r, path, opts, &config, &private, euid);
        }
        r
    }

    /// Files an agent must not read, write, or connect to.
    pub(super) fn private_paths(c: &Config) -> Vec<PathBuf> {
        let mut out = vec![c.audit.path.clone()];
        out.extend(c.credentials.file.clone());
        out.extend(c.admin.token_file.clone());
        if let AdminListen::Unix(p) = &c.admin.listen {
            out.push(p.clone());
        }
        out
    }

    /// Every existing component of `p` (as given and fully resolved), up to
    /// `/`, must be owned by `euid` or root; a component other than a
    /// symlink must not be group/world-writable unless it is a sticky
    /// directory. Otherwise another user could replace it after the check.
    pub(super) fn unsafe_components(p: &Path, euid: u32) -> Vec<String> {
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(p)
        };
        let mut out = Vec::new();
        for path in [abs, canon(p)] {
            for c in path.ancestors() {
                let Ok(m) = std::fs::symlink_metadata(c) else {
                    continue;
                };
                let owner_ok = m.uid() == euid || m.uid() == 0;
                let sticky_dir = m.is_dir() && m.mode() & 0o1000 != 0;
                let writable = !m.file_type().is_symlink() && m.mode() & 0o022 != 0;
                if !owner_ok || (writable && !sticky_dir) {
                    let msg = format!(
                        "{}: mode {:o} owner {}; must be owned by uid {euid} or root and not \
                         group/world-writable",
                        c.display(),
                        m.mode() & 0o7777,
                        m.uid()
                    );
                    if !out.contains(&msg) {
                        out.push(msg);
                    }
                }
            }
        }
        out
    }

    /// A secret file with a second name could be read through that name.
    fn single_link(path: &Path) -> Result<(), String> {
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.nlink() > 1 => Err(format!(
                "{}: has {} hard links; a secret file must have exactly one name",
                path.display(),
                m.nlink()
            )),
            _ => Ok(()),
        }
    }

    fn guard_files(r: &mut Report, c: &Config, euid: u32) {
        match crom_guard::CedarPdp::from_files(
            &c.policy.namespace,
            &c.policy.schema,
            &c.policy.policies,
        ) {
            Ok(_) => r.ok("policy", "policies validate against the schema"),
            Err(e) => r.fail("policy", e.to_string()),
        }
        match &c.credentials.file {
            Some(path) => match crom_guard::FileCredentialBroker::open(path)
                .map_err(|e| e.to_string())
                .and_then(|b| single_link(path).map(|()| b))
            {
                Ok(b) => r.ok(
                    "credentials",
                    format!(
                        "{}: 0600, owned by Guard, {} credential(s)",
                        path.display(),
                        b.credential_ids().len()
                    ),
                ),
                Err(e) => r.fail("credentials", e),
            },
            None => {
                let vars: Vec<&str> = c.credentials.env.iter().map(|e| e.var.as_str()).collect();
                match vars.iter().find(|v| std::env::var_os(v).is_none()) {
                    Some(v) => r.fail("credentials", format!("${v} is not set")),
                    None => r.ok(
                        "credentials",
                        format!("from Guard's environment: {}", vars.join(", ")),
                    ),
                }
            }
        }
        let token = match (&c.admin.token_file, &c.admin.token_env) {
            (Some(p), _) => crate::AdminToken::from_file(p)
                .map_err(|e| e.to_string())
                .and_then(|_| single_link(p)),
            (None, Some(v)) => crate::AdminToken::from_env(v)
                .map(|_| ())
                .map_err(|e| e.to_string()),
            (None, None) => unreachable!("validated"),
        };
        match token {
            Ok(()) => r.ok("admin-token", "admin token loads"),
            Err(e) => r.fail("admin-token", e),
        }
        let audit = &c.audit.path;
        match std::fs::symlink_metadata(audit) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                r.ok(
                    "audit-log",
                    format!("{} will be created 0600", audit.display()),
                );
            }
            Err(e) => r.fail("audit-log", format!("{}: {e}", audit.display())),
            Ok(m) if !m.file_type().is_file() => {
                r.fail(
                    "audit-log",
                    format!("{}: not a regular file", audit.display()),
                );
            }
            Ok(m) if m.mode() & 0o077 != 0 || m.uid() != euid => r.fail(
                "audit-log",
                format!(
                    "{}: mode {:o} owner {}; expected 0600 owned by uid {euid}",
                    audit.display(),
                    m.mode() & 0o777,
                    m.uid()
                ),
            ),
            Ok(_) => match single_link(audit).and_then(|()| {
                crom_guard::verify_file(audit, &[]).map_err(|e| format!("{}: {e}", audit.display()))
            }) {
                Ok(s) => r.ok(
                    "audit-log",
                    format!("chain verifies ({} records)", s.records),
                ),
                Err(e) => r.fail("audit-log", e),
            },
        }
    }

    /// Every directory holding Guard's state is owned by Guard and closed to
    /// group and others (0700), so no other uid can read, list, replace, or
    /// connect to anything in it.
    fn private_dirs(r: &mut Report, private: &[PathBuf], euid: u32) {
        let mut dirs: Vec<&Path> = private.iter().filter_map(|p| p.parent()).collect();
        dirs.sort();
        dirs.dedup();
        for dir in dirs {
            match std::fs::metadata(dir) {
                Ok(m) if m.uid() == euid && m.mode() & 0o077 == 0 => {
                    r.ok(
                        "state-dir",
                        format!("{}: 0700, owned by Guard", dir.display()),
                    );
                }
                Ok(m) => r.fail(
                    "state-dir",
                    format!(
                        "{}: mode {:o} owner {}; Guard's state directory must be 0700 owned \
                         by uid {euid}",
                        dir.display(),
                        m.mode() & 0o777,
                        m.uid()
                    ),
                ),
                Err(e) => r.fail("state-dir", format!("{}: {e}", dir.display())),
            }
        }
    }

    fn admin_listener(r: &mut Report, c: &Config, opts: &CheckOptions, euid: u32) {
        match &c.admin.listen {
            AdminListen::Unix(p) => match std::fs::symlink_metadata(p) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => r.ok(
                    "admin-socket",
                    format!("{}: Guard creates it 0600", p.display()),
                ),
                Err(e) => r.fail("admin-socket", format!("{}: {e}", p.display())),
                Ok(m)
                    if m.file_type().is_socket()
                        && m.mode() & 0o777 == 0o600
                        && m.uid() == euid =>
                {
                    r.ok(
                        "admin-socket",
                        format!("{}: 0600 socket owned by Guard", p.display()),
                    );
                }
                Ok(_) => r.fail(
                    "admin-socket",
                    format!("{}: not a 0600 socket owned by uid {euid}", p.display()),
                ),
            },
            // Any local uid can connect to a loopback port; only a unix
            // socket in a 0700 directory is out of a separate uid's reach.
            AdminListen::Tcp(a) if opts.agent_uid.is_some() => r.fail(
                "admin-socket",
                format!(
                    "admin.listen is TCP ({a}), reachable from the agent's uid; use \
                     unix:<path> in Guard's state directory"
                ),
            ),
            AdminListen::Tcp(a) => r.ok(
                "admin-socket",
                format!("admin.listen is TCP ({a}); the sandboxed agent cannot open sockets"),
            ),
        }
    }

    fn agent_uid(r: &mut Report, uid: u32, euid: u32) {
        if uid == 0 {
            r.fail(
                "agent-uid",
                "the agent runs as root and can read everything",
            );
        } else if uid == euid {
            r.fail(
                "agent-uid",
                format!(
                    "the agent runs as Guard's uid {euid}; use a separate user, or sandbox a \
                     Crom agent (--crom-settings)"
                ),
            );
        } else {
            r.ok(
                "agent-uid",
                format!("agent uid {uid} differs from Guard's uid {euid}"),
            );
        }
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(super) fn run_as(opts: &CheckOptions, euid: u32) -> Report {
        run(opts, euid)
    }
}

/// Checks on Crom's own configuration for an agent sandboxed on Guard's uid.
#[cfg(unix)]
mod crom {
    use super::unix::unsafe_components;
    use super::*;

    pub(super) fn check(
        r: &mut Report,
        path: &Path,
        opts: &CheckOptions,
        config: &Config,
        private: &[PathBuf],
        euid: u32,
    ) {
        if !cfg!(target_os = "linux") {
            r.fail(
                "platform",
                "the Bash sandbox is Linux-only; bypass guarantees do not hold here",
            );
        }
        let Some(ws) = workspace(r, opts, config, private, euid) else {
            return;
        };
        let config_dir = opts.crom_config_dir.clone().or_else(crom_config_dir);
        let settings = match settings_file(r, path, config_dir.as_deref(), &ws, euid) {
            Some(v) => v,
            None => return,
        };
        let sandbox = &settings["sandbox"];
        match sandbox["mode"].as_str() {
            Some("required") => r.ok("sandbox-mode", "sandbox.mode = \"required\""),
            other => r.fail(
                "sandbox-mode",
                format!(
                    "sandbox.mode is {}; it must be \"required\"",
                    other.map_or("unset (off)".into(), |m| format!("{m:?}"))
                ),
            ),
        }
        if sandbox["network"].as_bool() == Some(true) {
            r.fail(
                "sandbox-network",
                "sandbox.network = true lets agent Bash reach the business API directly",
            );
        } else {
            r.ok("sandbox-network", "agent Bash has no network");
        }
        let read_paths: Vec<PathBuf> = sandbox["read_paths"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();
        read_path_entries(r, &read_paths, &ws, euid);
        exposure(r, opts, &read_paths, private);
        env_allow(r, sandbox, config);

        let mut servers = Vec::new();
        if let Some(Value::Object(m)) = settings["mcp"].get("servers") {
            servers.extend(m.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        let mut files: Vec<PathBuf> = opts.crom_mcp_config.iter().cloned().collect();
        if let Some(f) = settings["mcp"]["config_file"].as_str() {
            files.push(PathBuf::from(f));
        }
        for f in files {
            if canon(&f).starts_with(&ws) {
                r.fail(
                    "mcp-servers",
                    format!("{}: MCP config inside the agent's workspace", f.display()),
                );
            }
            for p in unsafe_components(&f, euid) {
                r.fail("mcp-servers", p);
            }
            match read_json(&f) {
                Ok(v) => {
                    if let Some(Value::Object(m)) = v.get("mcpServers") {
                        servers.extend(m.iter().map(|(k, v)| (k.clone(), v.clone())));
                    }
                }
                Err(e) => r.fail("mcp-servers", e),
            }
        }
        if settings["mcp"]["enabled"].as_bool() == Some(false) {
            r.fail(
                "mcp-servers",
                "mcp.enabled = false: the agent cannot reach Guard",
            );
        }
        let guard = mcp_servers(r, &servers, config);
        if settings["plugins"]["enabled"].as_bool() == Some(false)
            || settings["plugins"]["allow_plugin_mcp"].as_bool() == Some(false)
        {
            r.ok("plugin-mcp", "plugins cannot add MCP servers");
        } else {
            r.fail(
                "plugin-mcp",
                "plugins may add MCP servers; set plugins.allow_plugin_mcp = false",
            );
        }
        if let Some(name) = guard {
            allow_rule(r, &settings, &name);
        }
    }

    fn read_json(path: &Path) -> Result<Value, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Guard's home directory: its `/etc/passwd` entry, else `$HOME`.
    fn guard_home(euid: u32) -> Option<PathBuf> {
        passwd_field(|_, uid| uid == euid.to_string(), 5)
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
    }

    /// The agent's workspace (required): it exists, is not trusted by Crom
    /// (a trusted `.agent/settings.json` or `.agent/mcp.json` could add
    /// servers and allow rules), and the sandbox's read-write grant on it
    /// reaches none of Guard's files, Guard's home, or Crom's own config.
    fn workspace(
        r: &mut Report,
        opts: &CheckOptions,
        config: &Config,
        private: &[PathBuf],
        euid: u32,
    ) -> Option<PathBuf> {
        let Some(given) = &opts.crom_workspace else {
            r.fail(
                "crom-workspace",
                "--crom-workspace is required with --crom-settings: the sandbox makes the \
                 workspace read-write",
            );
            return None;
        };
        let ws = match given.canonicalize() {
            Ok(w) => w,
            Err(e) => {
                r.fail("crom-workspace", format!("{}: {e}", given.display()));
                return None;
            }
        };
        let mut problems = Vec::new();
        let store = match &opts.crom_config_dir {
            Some(d) => Some(d.join(TRUST_STORE_FILE)),
            None => crom_trust_store(),
        };
        if let Some(store) = store {
            let trusted = std::fs::read(&store)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .and_then(|v| v["workspaces"].as_array().cloned())
                .unwrap_or_default();
            if trusted
                .iter()
                .filter_map(Value::as_str)
                .any(|p| Path::new(p) == ws)
            {
                problems.push(format!(
                    "{} is trusted in {}; its .agent/settings.json and .agent/mcp.json \
                     would apply (crom trust --revoke)",
                    ws.display(),
                    store.display()
                ));
            }
        }
        // Either containing the other is unsafe for these.
        let mut two_way: Vec<PathBuf> = private.iter().map(|p| canon(p)).collect();
        two_way.extend(
            private
                .iter()
                .filter_map(|p| canon(p).parent().map(Path::to_path_buf)),
        );
        for p in [&opts.config, &config.policy.schema, &config.policy.policies] {
            two_way.extend(canon(p).parent().map(Path::to_path_buf));
        }
        let config_dir = opts.crom_config_dir.clone().or_else(crom_config_dir);
        two_way.extend(config_dir.map(|d| canon(&d)));
        for g in two_way {
            if g.starts_with(&ws) || ws.starts_with(&g) {
                problems.push(format!(
                    "the agent workspace {} (read-write in the sandbox) overlaps {}",
                    ws.display(),
                    g.display()
                ));
            }
        }
        if let Some(home) = guard_home(euid).map(|h| canon(&h)) {
            if home.starts_with(&ws) {
                problems.push(format!(
                    "the agent workspace {} contains Guard's home {}",
                    ws.display(),
                    home.display()
                ));
            }
        }
        r.all(
            "crom-workspace",
            problems,
            "the workspace is untrusted and reaches none of Guard's files",
        );
        Some(ws)
    }

    /// The settings file must be the one Crom reads (its user settings
    /// path), owned safely, and outside the workspace.
    fn settings_file(
        r: &mut Report,
        path: &Path,
        config_dir: Option<&Path>,
        ws: &Path,
        euid: u32,
    ) -> Option<Value> {
        let settings = match read_json(path) {
            Ok(v) => v,
            Err(e) => {
                r.fail("crom-settings", e);
                return None;
            }
        };
        let mut problems = unsafe_components(path, euid);
        let given = canon(path);
        match config_dir.map(|d| canon(&d.join("settings.json"))) {
            Some(expected) if expected == given => {}
            Some(expected) => problems.push(format!(
                "{} is not Crom's user settings file {}; check the file Crom will read \
                 (CROM_CONFIG_HOME / XDG_CONFIG_HOME / HOME as the agent sees them)",
                path.display(),
                expected.display()
            )),
            None => problems.push("cannot resolve Crom's config directory".into()),
        }
        if given.starts_with(ws) {
            problems.push(format!(
                "{} is inside the agent's workspace",
                path.display()
            ));
        }
        r.all("crom-settings", problems, "Crom's user settings file");
        Some(settings)
    }

    /// Crom re-resolves `read_paths` on every Bash call, following
    /// symlinks, so each entry must be a real path no one but Guard or root
    /// can change.
    fn read_path_entries(r: &mut Report, entries: &[PathBuf], ws: &Path, euid: u32) {
        let mut problems = Vec::new();
        for p in entries {
            match std::fs::symlink_metadata(p) {
                Err(e) => problems.push(format!("sandbox.read_paths {}: {e}", p.display())),
                Ok(m) if m.file_type().is_symlink() => {
                    problems.push(format!("sandbox.read_paths {} is a symlink", p.display()))
                }
                Ok(_) => {}
            }
            if !p.is_absolute() {
                problems.push(format!("sandbox.read_paths {} is relative", p.display()));
            }
            if canon(p).starts_with(ws) {
                problems.push(format!(
                    "sandbox.read_paths {} is inside the agent's workspace",
                    p.display()
                ));
            }
            problems.extend(
                unsafe_components(p, euid)
                    .into_iter()
                    .map(|m| format!("sandbox.read_paths {}: {m}", p.display())),
            );
        }
        r.all(
            "sandbox-read-paths",
            problems,
            "every sandbox.read_paths entry is a fixed path only Guard or root can change",
        );
    }

    /// Nothing the sandbox can read (built-in set or `read_paths`) covers
    /// Guard's secret files or the directories holding them.
    fn exposure(r: &mut Report, opts: &CheckOptions, read_paths: &[PathBuf], private: &[PathBuf]) {
        let builtin = opts
            .sandbox_read_paths
            .clone()
            .unwrap_or_else(sandbox_builtin_read_paths);
        let secrets: Vec<PathBuf> = private.iter().map(|p| canon(p)).collect();
        let mut exposed = Vec::new();
        for root in &builtin {
            let root = canon(root);
            for s in &secrets {
                if s.starts_with(&root) {
                    exposed.push(format!(
                        "{} is under {}, which the sandbox always lets Bash read",
                        s.display(),
                        root.display()
                    ));
                }
            }
        }
        for p in read_paths {
            let root = canon(p);
            for s in &secrets {
                let dir = s.parent().unwrap_or(s);
                if s.starts_with(&root) || root.starts_with(dir) {
                    exposed.push(format!(
                        "sandbox.read_paths entry {} exposes {}",
                        p.display(),
                        s.display()
                    ));
                }
            }
        }
        r.all(
            "sandbox-paths",
            exposed,
            "no sandbox read path reaches Guard's state",
        );
    }

    fn env_allow(r: &mut Report, sandbox: &Value, config: &Config) {
        let mut secret_vars: Vec<&str> = config
            .credentials
            .env
            .iter()
            .map(|e| e.var.as_str())
            .collect();
        secret_vars.extend(config.admin.token_env.as_deref());
        let allowed: Vec<&str> = sandbox["env_allow"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let leaked: Vec<&str> = allowed
            .into_iter()
            .filter(|v| secret_vars.contains(v))
            .collect();
        if leaked.is_empty() {
            r.ok(
                "sandbox-env",
                "sandbox.env_allow passes none of Guard's secret variables",
            );
        } else {
            r.fail(
                "sandbox-env",
                format!(
                    "sandbox.env_allow passes Guard's secret variables: {}",
                    leaked.join(", ")
                ),
            );
        }
    }

    /// Exactly one MCP server: Streamable HTTP, `guard: true`, at Guard's
    /// agent endpoint. Returns its name when that holds.
    fn mcp_servers(r: &mut Report, servers: &[(String, Value)], config: &Config) -> Option<String> {
        let [(name, server)] = servers else {
            let names: Vec<&str> = servers.iter().map(|(n, _)| n.as_str()).collect();
            r.fail(
                "mcp-servers",
                format!(
                    "Crom must configure exactly one MCP server, Guard; found {}: [{}]",
                    servers.len(),
                    names.join(", ")
                ),
            );
            return None;
        };
        let kind = server["type"].as_str().unwrap_or("");
        if !matches!(kind, "http" | "streamable_http" | "streamable-http") {
            r.fail(
                "mcp-servers",
                format!("MCP server {name:?} has type {kind:?}; only Guard over http is allowed"),
            );
            return None;
        }
        if server["guard"].as_bool() != Some(true) {
            r.fail(
                "mcp-servers",
                format!("MCP server {name:?} is not marked \"guard\": true (docs/sdk/guard.md)"),
            );
            return None;
        }
        let url = server["url"].as_str().unwrap_or("");
        if !points_at(url, config.agent.listen) {
            r.fail(
                "mcp-servers",
                format!(
                    "MCP server {name:?} url {url:?} is not Guard's agent endpoint \
                     http://{}{}",
                    config.agent.listen,
                    crate::MCP_PATH
                ),
            );
            return None;
        }
        r.ok(
            "mcp-servers",
            format!("the only MCP server is Guard ({name:?})"),
        );
        Some(name.clone())
    }

    fn points_at(url: &str, listen: std::net::SocketAddr) -> bool {
        let Some(rest) = url.strip_prefix("http://") else {
            return false;
        };
        let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        if path.trim_end_matches('/') != crate::MCP_PATH {
            return false;
        }
        let Some((host, port)) = authority.rsplit_once(':') else {
            return false;
        };
        let loopback = matches!(host, "localhost" | "[::1]")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        listen.port() != 0
            && port.parse() == Ok(listen.port())
            && loopback
            && listen.ip().is_loopback()
    }

    fn allow_rule(r: &mut Report, settings: &Value, server: &str) {
        let prefix = format!("mcp__{server}__");
        let present = settings["allowed_tools"].as_array().is_some_and(|rules| {
            rules.iter().any(|rule| match rule {
                Value::String(s) => s.starts_with(&prefix),
                Value::Object(o) => o.get("server").and_then(Value::as_str) == Some(server),
                _ => false,
            })
        });
        if present {
            r.ok(
                "guard-allow-rule",
                format!("allowed_tools has a rule for {prefix}*"),
            );
        } else {
            r.fail(
                "guard-allow-rule",
                format!("allowed_tools has no rule for Guard's tools; add \"{prefix}*\""),
            );
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use serde_json::json;

    use super::*;

    const SCHEMA: &str =
        include_str!("../../../examples/guard-support/policies/support.cedarschema");
    const POLICIES: &str = include_str!("../../../examples/guard-support/policies/support.cedar");

    struct Fixture {
        dir: tempfile::TempDir,
        state: PathBuf,
        opts: CheckOptions,
    }

    fn mode(p: &Path, m: u32) {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
    }

    fn write(p: &Path, text: &str, m: u32) {
        std::fs::write(p, text).unwrap();
        mode(p, m);
    }

    fn settings() -> Value {
        json!({
            "sandbox": {"mode": "required", "network": false},
            "mcp": {"enabled": true, "servers": {"guard": {
                "type": "http",
                "url": "http://127.0.0.1:8470/mcp",
                "headers": {"Authorization": "Bearer ${CROM_GUARD_HANDLE}"},
                "guard": true
            }}},
            "plugins": {"allow_plugin_mcp": false},
            "allowed_tools": ["mcp__guard__*"]
        })
    }

    /// The reference isolation profile: Guard's state in `<tmp>/<parent>/state`
    /// (0700), Crom's config in `<tmp>/crom`, the workspace in
    /// `<tmp>/workspace`, and the sandbox reading only `/usr` and `/etc`.
    fn fixture_at(parent: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join(parent).join("state");
        std::fs::create_dir_all(state.join("policies")).unwrap();
        mode(&state, 0o700);
        write(&state.join("policies/support.cedarschema"), SCHEMA, 0o644);
        write(&state.join("policies/support.cedar"), POLICIES, 0o644);
        write(
            &state.join("guard.toml"),
            include_str!("../../../examples/guard-support/guard.toml"),
            0o644,
        );
        write(&state.join("admin.token"), &"t".repeat(40), 0o600);
        write(
            &state.join("credentials.json"),
            r#"{"credentials": [{"id": "business-api", "secret": "sk-x", "actions": ["refund.issue"]}]}"#,
            0o600,
        );
        let crom = dir.path().join("crom");
        std::fs::create_dir(&crom).unwrap();
        std::fs::write(crom.join("settings.json"), settings().to_string()).unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir(&ws).unwrap();
        let opts = CheckOptions {
            config: state.join("guard.toml"),
            crom_settings: Some(crom.join("settings.json")),
            crom_workspace: Some(ws),
            crom_config_dir: Some(crom),
            sandbox_read_paths: Some(vec!["/usr".into(), "/etc".into()]),
            ..CheckOptions::default()
        };
        Fixture { dir, state, opts }
    }

    fn fixture() -> Fixture {
        fixture_at("srv")
    }

    impl Fixture {
        fn state(&self) -> PathBuf {
            self.state.clone()
        }

        fn run(&self) -> Report {
            unix::run_as(&self.opts, rustix::process::geteuid().as_raw())
        }

        fn set_settings(&self, v: Value) {
            std::fs::write(self.opts.crom_settings.as_ref().unwrap(), v.to_string()).unwrap();
        }

        fn set_config(&self, from: &str, to: &str) {
            let text = std::fs::read_to_string(&self.opts.config).unwrap();
            assert!(text.contains(from), "{from}");
            std::fs::write(&self.opts.config, text.replace(from, to)).unwrap();
        }

        /// Asserts that exactly the `expected` checks fail.
        fn fails_with(&self, expected: &[&str]) {
            let r = self.run();
            assert_eq!(r.failed(), expected, "{:#?}", r.findings);
        }
    }

    /// The reference isolation profile passes every check.
    #[test]
    fn reference_profile_passes() {
        let f = fixture();
        let r = f.run();
        assert!(r.passed(), "{:#?}", r.findings);
    }

    /// The reference profile also passes with the sandbox read set computed from this environment.
    #[test]
    fn reference_profile_passes_with_computed_read_set() {
        let mut f = fixture();
        f.opts.sandbox_read_paths = None;
        let r = f.run();
        assert!(r.passed(), "{:#?}", r.findings);
    }

    /// Guard running as root fails `guard-user`.
    #[test]
    fn root_guard_fails() {
        let f = fixture();
        let r = unix::run_as(&f.opts, 0);
        assert!(r.failed().contains(&"guard-user"), "{:#?}", r.findings);
    }

    /// An unreadable or invalid config stops the check with `config`.
    #[test]
    fn invalid_config_fails() {
        let f = fixture();
        std::fs::write(&f.opts.config, "[agent]\nlisten = \"0.0.0.0:1\"\n").unwrap();
        f.fails_with(&["config"]);
    }

    /// An agent endpoint on port 0 fails `agent-listen`, and no MCP URL matches it.
    #[test]
    fn agent_port_zero_fails() {
        let f = fixture();
        f.set_config("listen = \"127.0.0.1:8470\"", "listen = \"127.0.0.1:0\"");
        f.fails_with(&["agent-listen", "mcp-servers"]);
    }

    /// A group-writable config file fails `config-file`.
    #[test]
    fn writable_config_file_fails() {
        let f = fixture();
        mode(&f.opts.config, 0o664);
        f.fails_with(&["config-file"]);
    }

    /// A non-sticky world-writable ancestor of Guard's state fails `config-file`, `policy-files` and `state-path`; a sticky one passes.
    #[test]
    fn writable_ancestor_fails_unless_sticky() {
        let f = fixture();
        let srv = f.dir.path().join("srv");
        mode(&srv, 0o777);
        f.fails_with(&["config-file", "policy-files", "state-path"]);
        mode(&srv, 0o1777);
        f.fails_with(&[]);
    }

    /// A symlinked state directory is checked along its resolved path too.
    #[test]
    fn symlinked_state_is_resolved() {
        let f = fixture();
        let open = f.dir.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::rename(f.state(), open.join("state")).unwrap();
        std::os::unix::fs::symlink(open.join("state"), f.state()).unwrap();
        f.fails_with(&[]);
        mode(&open, 0o777);
        f.fails_with(&["config-file", "policy-files", "state-path"]);
    }

    /// A policy set that does not validate against the schema fails `policy`.
    #[test]
    fn invalid_policy_fails() {
        let f = fixture();
        write(
            &f.state().join("policies/support.cedar"),
            "permit (principal, action == Support::Action::\"nope\", resource);",
            0o644,
        );
        f.fails_with(&["policy"]);
    }

    /// A world-writable policy file fails `policy-files`.
    #[test]
    fn writable_policy_file_fails() {
        let f = fixture();
        mode(&f.state().join("policies/support.cedar"), 0o666);
        f.fails_with(&["policy-files"]);
    }

    /// A group-readable credentials file fails `credentials`.
    #[test]
    fn readable_credentials_fail() {
        let f = fixture();
        mode(&f.state().join("credentials.json"), 0o640);
        f.fails_with(&["credentials"]);
    }

    /// A credentials file with a second hard link fails `credentials`.
    #[test]
    fn hard_linked_credentials_fail() {
        let f = fixture();
        std::fs::hard_link(
            f.state().join("credentials.json"),
            f.dir.path().join("copy.json"),
        )
        .unwrap();
        f.fails_with(&["credentials"]);
    }

    /// An env credential whose variable is unset fails `credentials`.
    #[test]
    fn missing_env_credential_fails() {
        let f = fixture();
        f.set_config(
            "[credentials]\nfile = \"credentials.json\"",
            "[[credentials.env]]\nid = \"business-api\"\nvar = \"CROM_GUARD_CHECK_TEST_UNSET\"\nactions = [\"refund.issue\"]",
        );
        f.fails_with(&["credentials"]);
    }

    /// A world-readable admin token fails `admin-token`.
    #[test]
    fn readable_admin_token_fails() {
        let f = fixture();
        mode(&f.state().join("admin.token"), 0o644);
        f.fails_with(&["admin-token"]);
    }

    /// A world-readable or broken audit log fails `audit-log`.
    #[test]
    fn unsafe_audit_log_fails() {
        let f = fixture();
        let audit = f.state().join("audit.jsonl");
        write(&audit, "", 0o644);
        f.fails_with(&["audit-log"]);
        write(&audit, "not a record\n", 0o600);
        f.fails_with(&["audit-log"]);
    }

    /// A state directory open to group or others fails `state-dir`.
    #[test]
    fn open_state_dir_fails() {
        let f = fixture();
        mode(&f.state(), 0o750);
        f.fails_with(&["state-dir"]);
    }

    /// An admin socket path that exists but is not a 0600 socket fails `admin-socket`.
    #[test]
    fn wrong_admin_socket_fails() {
        let f = fixture();
        write(&f.state().join("admin.sock"), "", 0o600);
        f.fails_with(&["admin-socket"]);
    }

    /// A TCP admin listener fails `admin-socket` when the agent has its own uid.
    #[test]
    fn tcp_admin_with_separate_uid_fails() {
        let mut f = fixture();
        f.set_config("unix:admin.sock", "127.0.0.1:8471");
        let r = f.run();
        assert!(
            r.passed(),
            "TCP admin with a sandboxed agent passes: {:#?}",
            r.findings
        );
        f.opts.crom_settings = None;
        f.opts.agent_uid = Some(rustix::process::geteuid().as_raw() + 1);
        f.fails_with(&["admin-socket"]);
    }

    /// Declaring no agent isolation at all fails `agent-isolation`.
    #[test]
    fn no_agent_isolation_fails() {
        let mut f = fixture();
        f.opts.crom_settings = None;
        f.fails_with(&["agent-isolation"]);
    }

    /// An agent uid equal to Guard's, or root, fails `agent-uid`; another uid passes.
    #[test]
    fn agent_uid_must_differ() {
        let mut f = fixture();
        f.opts.crom_settings = None;
        let euid = rustix::process::geteuid().as_raw();
        f.opts.agent_uid = Some(euid);
        f.fails_with(&["agent-uid"]);
        f.opts.agent_uid = Some(0);
        f.fails_with(&["agent-uid"]);
        f.opts.agent_uid = Some(euid + 1);
        f.fails_with(&[]);
    }

    /// Crom settings that do not parse fail `crom-settings`.
    #[test]
    fn unparsable_settings_fail() {
        let f = fixture();
        std::fs::write(f.opts.crom_settings.as_ref().unwrap(), "{").unwrap();
        f.fails_with(&["crom-settings"]);
    }

    /// A settings file other than Crom's user settings path fails `crom-settings`.
    #[test]
    fn settings_outside_crom_config_dir_fail() {
        let mut f = fixture();
        let other = f.dir.path().join("other.json");
        std::fs::write(&other, settings().to_string()).unwrap();
        f.opts.crom_settings = Some(other);
        f.fails_with(&["crom-settings"]);
    }

    /// Omitting `--crom-workspace` with `--crom-settings` fails `crom-workspace`.
    #[test]
    fn workspace_is_required() {
        let mut f = fixture();
        f.opts.crom_workspace = None;
        f.fails_with(&["crom-workspace"]);
    }

    /// A workspace containing Guard's state, inside it, or containing Crom's config fails `crom-workspace`.
    #[test]
    fn workspace_overlap_fails() {
        let mut f = fixture();
        for ws in [
            f.dir.path().to_path_buf(),
            f.dir.path().join("srv"),
            f.state().join("policies"),
        ] {
            f.opts.crom_workspace = Some(ws.clone());
            let r = f.run();
            assert!(
                r.failed().contains(&"crom-workspace"),
                "{ws:?}: {:#?}",
                r.findings
            );
        }
        f.opts.crom_workspace = Some(f.dir.path().join("crom"));
        let r = f.run();
        assert!(r.failed().contains(&"crom-workspace"), "{:#?}", r.findings);
    }

    /// A workspace that is an ancestor of Guard's home fails `crom-workspace`.
    #[test]
    fn workspace_containing_guard_home_fails() {
        let mut f = fixture();
        f.opts.crom_workspace = Some("/".into());
        let r = f.run();
        assert!(r.failed().contains(&"crom-workspace"), "{:#?}", r.findings);
    }

    /// A workspace Crom trusts fails `crom-workspace`.
    #[test]
    fn trusted_workspace_fails() {
        let f = fixture();
        let ws = f
            .opts
            .crom_workspace
            .clone()
            .unwrap()
            .canonicalize()
            .unwrap();
        std::fs::write(
            f.dir.path().join("crom").join(TRUST_STORE_FILE),
            json!({"workspaces": [ws]}).to_string(),
        )
        .unwrap();
        f.fails_with(&["crom-workspace"]);
    }

    /// A sandbox mode other than `required` (including unset) fails `sandbox-mode`.
    #[test]
    fn sandbox_not_required_fails() {
        let f = fixture();
        let mut s = settings();
        s["sandbox"]["mode"] = json!("off");
        f.set_settings(s.clone());
        f.fails_with(&["sandbox-mode"]);
        s["sandbox"].as_object_mut().unwrap().remove("mode");
        f.set_settings(s);
        f.fails_with(&["sandbox-mode"]);
    }

    /// Granting Bash network fails `sandbox-network`.
    #[test]
    fn sandbox_network_fails() {
        let f = fixture();
        let mut s = settings();
        s["sandbox"]["network"] = json!(true);
        f.set_settings(s);
        f.fails_with(&["sandbox-network"]);
    }

    /// Guard state under a built-in sandbox read root (an /etc-like root) fails `sandbox-paths`.
    #[test]
    fn state_under_builtin_root_fails() {
        let mut f = fixture_at("etc");
        f.opts.sandbox_read_paths = Some(vec!["/usr".into(), f.dir.path().join("etc")]);
        f.fails_with(&["sandbox-paths"]);
    }

    /// Guard state under `$CARGO_HOME/registry`, which the sandbox reads, fails `sandbox-paths`.
    #[test]
    fn state_under_cargo_home_fails() {
        let mut f = fixture_at("cargo/registry");
        let cargo = f.dir.path().join("cargo");
        let read = builtin_read_paths_with(|k| match k {
            "CARGO_HOME" => Some(cargo.clone().into_os_string()),
            _ => None,
        });
        assert!(read.contains(&cargo.join("registry")), "{read:?}");
        f.opts.sandbox_read_paths = Some(read);
        f.fails_with(&["sandbox-paths"]);
    }

    /// A sandbox read path covering Guard's state directory fails `sandbox-paths`.
    #[test]
    fn read_path_over_state_fails() {
        let f = fixture();
        let mut s = settings();
        s["sandbox"]["read_paths"] = json!([f.dir.path().join("srv")]);
        f.set_settings(s);
        f.fails_with(&["sandbox-paths"]);
    }

    /// A safe, Guard-owned read path passes.
    #[test]
    fn safe_read_path_passes() {
        let f = fixture();
        let data = f.dir.path().join("data");
        std::fs::create_dir(&data).unwrap();
        let mut s = settings();
        s["sandbox"]["read_paths"] = json!([data]);
        f.set_settings(s);
        f.fails_with(&[]);
    }

    /// A read path that is missing, a symlink, relative, inside the workspace, or under a writable directory fails `sandbox-read-paths`.
    #[test]
    fn unsafe_read_paths_fail() {
        let f = fixture();
        let open = f.dir.path().join("open");
        std::fs::create_dir_all(open.join("data")).unwrap();
        mode(&open, 0o777);
        let link = f.dir.path().join("link");
        std::os::unix::fs::symlink(f.dir.path().join("crom"), &link).unwrap();
        let in_ws = f.opts.crom_workspace.clone().unwrap().join("d");
        std::fs::create_dir(&in_ws).unwrap();
        for p in [
            json!(f.dir.path().join("missing")),
            json!(link),
            json!("relative/dir"),
            json!(in_ws),
            json!(open.join("data")),
        ] {
            let mut s = settings();
            s["sandbox"]["read_paths"] = json!([p.clone()]);
            f.set_settings(s);
            let r = f.run();
            assert!(
                r.failed().contains(&"sandbox-read-paths"),
                "{p}: {:#?}",
                r.findings
            );
        }
    }

    /// Passing Guard's admin-token variable into the sandbox fails `sandbox-env`.
    #[test]
    fn env_allow_of_guard_secret_fails() {
        let f = fixture();
        f.set_config(
            "token_file = \"admin.token\"",
            "token_env = \"CROM_GUARD_CHECK_TOKEN\"",
        );
        let mut s = settings();
        s["sandbox"]["env_allow"] = json!(["CROM_GUARD_CHECK_TOKEN"]);
        f.set_settings(s);
        let r = f.run();
        assert!(r.failed().contains(&"sandbox-env"), "{:#?}", r.findings);
    }

    /// A second MCP server, from settings or an MCP config file, fails `mcp-servers`.
    #[test]
    fn second_mcp_server_fails() {
        let mut f = fixture();
        let mut s = settings();
        s["mcp"]["servers"]["billing"] = json!({"type": "http", "url": "http://127.0.0.1:9/mcp"});
        f.set_settings(s);
        f.fails_with(&["mcp-servers"]);

        f.set_settings(settings());
        let extra = f.dir.path().join("mcp.json");
        std::fs::write(
            &extra,
            json!({"mcpServers": {"refunds": {"type": "http", "url": "http://127.0.0.1:9/mcp"}}})
                .to_string(),
        )
        .unwrap();
        f.opts.crom_mcp_config = Some(extra);
        f.fails_with(&["mcp-servers"]);
    }

    /// An MCP config file inside the agent's workspace fails `mcp-servers`.
    #[test]
    fn mcp_config_in_workspace_fails() {
        let mut f = fixture();
        let file = f.opts.crom_workspace.clone().unwrap().join("mcp.json");
        std::fs::write(&file, json!({"mcpServers": {}}).to_string()).unwrap();
        f.opts.crom_mcp_config = Some(file);
        f.fails_with(&["mcp-servers"]);
    }

    /// A stdio MCP server, even as the only one, fails `mcp-servers`.
    #[test]
    fn stdio_server_fails() {
        let f = fixture();
        let mut s = settings();
        s["mcp"]["servers"] = json!({"guard": {"type": "stdio", "command": "crom-guard"}});
        f.set_settings(s);
        f.fails_with(&["mcp-servers"]);
    }

    /// A Guard server not marked `"guard": true` fails `mcp-servers`.
    #[test]
    fn unmarked_guard_server_fails() {
        let f = fixture();
        let mut s = settings();
        s["mcp"]["servers"]["guard"]
            .as_object_mut()
            .unwrap()
            .remove("guard");
        f.set_settings(s);
        f.fails_with(&["mcp-servers"]);
    }

    /// An MCP server URL that is not Guard's agent endpoint fails `mcp-servers`.
    #[test]
    fn wrong_guard_url_fails() {
        let f = fixture();
        for url in [
            "http://127.0.0.1:8480/mcp",
            "http://10.0.0.1:8470/mcp",
            "http://127.0.0.1:8470/other",
            "https://127.0.0.1:8470/mcp",
        ] {
            let mut s = settings();
            s["mcp"]["servers"]["guard"]["url"] = json!(url);
            f.set_settings(s);
            f.fails_with(&["mcp-servers"]);
        }
    }

    /// Disabling MCP fails `mcp-servers`: the agent could not reach Guard.
    #[test]
    fn mcp_disabled_fails() {
        let f = fixture();
        let mut s = settings();
        s["mcp"]["enabled"] = json!(false);
        f.set_settings(s);
        f.fails_with(&["mcp-servers"]);
    }

    /// Leaving plugin MCP servers allowed (the default) fails `plugin-mcp`.
    #[test]
    fn plugin_mcp_allowed_fails() {
        let f = fixture();
        let mut s = settings();
        s.as_object_mut().unwrap().remove("plugins");
        f.set_settings(s);
        f.fails_with(&["plugin-mcp"]);
    }

    /// Missing an allow rule for Guard's tools fails `guard-allow-rule`; an object rule passes.
    #[test]
    fn guard_allow_rule_required() {
        let f = fixture();
        let mut s = settings();
        s["allowed_tools"] = json!(["Read"]);
        f.set_settings(s.clone());
        f.fails_with(&["guard-allow-rule"]);
        s["allowed_tools"] = json!([{"server": "guard", "tool_glob": "*"}]);
        f.set_settings(s);
        f.fails_with(&[]);
    }

    /// `resolve_user` accepts numeric uids and names from /etc/passwd.
    #[test]
    fn resolve_user_accepts_uid_and_name() {
        assert_eq!(resolve_user("1234").unwrap(), 1234);
        assert_eq!(resolve_user("root").unwrap(), 0);
        assert!(resolve_user("no-such-user-crom-guard").is_err());
    }
}
