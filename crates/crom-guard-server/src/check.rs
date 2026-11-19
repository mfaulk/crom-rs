//! `crom-guard check`: the isolation-profile preflight
//! (`docs/guard/deployment.md`). It reads files and metadata only: it binds
//! nothing, runs no agent, and never prints a secret. Crom's settings are
//! read as plain JSON so this crate stays free of other `crom-*` crates.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::{AdminListen, Config};

/// What to check. Paths are as given on the command line.
#[derive(Debug, Clone, Default)]
pub struct CheckOptions {
    pub config: PathBuf,
    /// Crom's settings file for an agent sandboxed by Crom on Guard's uid.
    pub crom_settings: Option<PathBuf>,
    /// An MCP config file Crom is started with (`--mcp-config`).
    pub crom_mcp_config: Option<PathBuf>,
    /// The directory Crom's agent works in (read-write inside the sandbox).
    pub crom_workspace: Option<PathBuf>,
    /// Uid of an agent running under its own user.
    pub agent_uid: Option<u32>,
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

    /// Findings that failed, by check id.
    pub fn failed(&self) -> Vec<&'static str> {
        self.findings
            .iter()
            .filter(|f| !f.passed)
            .map(|f| f.check)
            .collect()
    }

    fn ok(&mut self, check: &'static str, detail: impl Into<String>) {
        self.push(check, true, detail);
    }

    fn fail(&mut self, check: &'static str, detail: impl Into<String>) {
        self.push(check, false, detail);
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
    let passwd = std::fs::read_to_string("/etc/passwd").map_err(|e| format!("/etc/passwd: {e}"))?;
    passwd
        .lines()
        .filter_map(|l| {
            let mut f = l.split(':');
            Some((f.next()?, f.nth(1)?))
        })
        .find(|(name, _)| *name == user)
        .and_then(|(_, uid)| uid.parse().ok())
        .ok_or_else(|| format!("no user {user:?} in /etc/passwd"))
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
        not_writable_by_others(&mut r, "config-file", &opts.config, euid);
        guard_files(&mut r, &config, euid);
        let private = private_paths(&config);
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
            super::crom::check(&mut r, path, opts, &config, &private);
        }
        r
    }

    /// Files and directories an agent must not read, write, or connect to.
    fn private_paths(c: &Config) -> Vec<PathBuf> {
        let mut out = vec![c.audit.path.clone()];
        out.extend(c.credentials.file.clone());
        out.extend(c.admin.token_file.clone());
        if let AdminListen::Unix(p) = &c.admin.listen {
            out.push(p.clone());
        }
        out
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
        for p in [&c.policy.schema, &c.policy.policies] {
            not_writable_by_others(r, "policy-files", p, euid);
            if let Some(dir) = p.parent() {
                not_writable_by_others(r, "policy-files", dir, euid);
            }
        }
        match &c.credentials.file {
            Some(path) => match crom_guard::FileCredentialBroker::open(path) {
                Ok(b) => r.ok(
                    "credentials",
                    format!(
                        "{}: 0600, owned by Guard, {} credential(s)",
                        path.display(),
                        b.credential_ids().len()
                    ),
                ),
                Err(e) => r.fail("credentials", e.to_string()),
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
            (Some(p), _) => crate::AdminToken::from_file(p),
            (None, Some(v)) => crate::AdminToken::from_env(v),
            (None, None) => unreachable!("validated"),
        };
        match token {
            Ok(_) => r.ok("admin-token", "admin token loads"),
            Err(e) => r.fail("admin-token", e.to_string()),
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
            Ok(_) => match crom_guard::verify_file(audit, &[]) {
                Ok(s) => r.ok(
                    "audit-log",
                    format!("chain verifies ({} records)", s.records),
                ),
                Err(e) => r.fail("audit-log", format!("{}: {e}", audit.display())),
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

    pub(super) fn not_writable_by_others(r: &mut Report, check: &'static str, p: &Path, euid: u32) {
        match std::fs::metadata(p) {
            Ok(m) if (m.uid() == euid || m.uid() == 0) && m.mode() & 0o022 == 0 => {}
            Ok(m) => r.fail(
                check,
                format!(
                    "{}: mode {:o} owner {}; must be owned by Guard or root and not \
                     group/world-writable",
                    p.display(),
                    m.mode() & 0o777,
                    m.uid()
                ),
            ),
            Err(e) => r.fail(check, format!("{}: {e}", p.display())),
        }
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(super) fn run_as(opts: &CheckOptions, euid: u32) -> Report {
        run(opts, euid)
    }
}

/// Checks on Crom's own configuration for an agent sandboxed on Guard's uid.
mod crom {
    use super::*;

    pub(super) fn check(
        r: &mut Report,
        path: &Path,
        opts: &CheckOptions,
        config: &Config,
        private: &[PathBuf],
    ) {
        let settings = match read_json(path) {
            Ok(v) => v,
            Err(e) => return r.fail("crom-settings", e),
        };
        r.ok("crom-settings", format!("{} parses", path.display()));
        if !cfg!(target_os = "linux") {
            r.fail(
                "platform",
                "the Bash sandbox is Linux-only; bypass guarantees do not hold here",
            );
        }
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
        read_paths(r, sandbox, opts, private);
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

    fn absolute(p: &Path) -> PathBuf {
        std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
    }

    /// True when either path contains the other.
    fn overlaps(a: &Path, b: &Path) -> bool {
        let (a, b) = (absolute(a), absolute(b));
        a.starts_with(&b) || b.starts_with(&a)
    }

    fn read_paths(r: &mut Report, sandbox: &Value, opts: &CheckOptions, private: &[PathBuf]) {
        let mut exposed = Vec::new();
        let extra = sandbox["read_paths"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for p in extra.iter().filter_map(Value::as_str) {
            for g in private {
                if overlaps(Path::new(p), g) {
                    exposed.push(format!(
                        "sandbox.read_paths entry {p} exposes {}",
                        g.display()
                    ));
                }
            }
        }
        if let Some(ws) = &opts.crom_workspace {
            for g in private {
                if overlaps(ws, g) {
                    exposed.push(format!(
                        "the agent workspace {} (read-write) overlaps {}",
                        ws.display(),
                        g.display()
                    ));
                }
            }
        }
        if exposed.is_empty() {
            r.ok("sandbox-paths", "no sandbox path reaches Guard's state");
        } else {
            r.fail("sandbox-paths", exposed.join("; "));
        }
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

    /// Exactly one MCP server, Streamable HTTP, at Guard's agent endpoint.
    /// Returns its name when that holds.
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
        let port_ok = listen.port() == 0 || port.parse() == Ok(listen.port());
        loopback && port_ok && listen.ip().is_loopback()
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
                "headers": {"Authorization": "Bearer ${CROM_GUARD_HANDLE}"}
            }}},
            "plugins": {"allow_plugin_mcp": false},
            "allowed_tools": ["mcp__guard__*"]
        })
    }

    /// The reference isolation profile: a 0700 state directory with the
    /// support config, and a Crom agent sandboxed on Guard's uid.
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir(&state).unwrap();
        mode(&state, 0o700);
        std::fs::create_dir(state.join("policies")).unwrap();
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
        let settings_path = dir.path().join("settings.json");
        std::fs::write(&settings_path, settings().to_string()).unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir(&ws).unwrap();
        let opts = CheckOptions {
            config: state.join("guard.toml"),
            crom_settings: Some(settings_path),
            crom_workspace: Some(ws),
            ..CheckOptions::default()
        };
        Fixture { dir, opts }
    }

    impl Fixture {
        fn state(&self) -> PathBuf {
            self.dir.path().join("state")
        }

        fn run(&self) -> Report {
            unix::run_as(&self.opts, rustix::process::geteuid().as_raw())
        }

        fn set_settings(&self, v: Value) {
            std::fs::write(self.opts.crom_settings.as_ref().unwrap(), v.to_string()).unwrap();
        }

        /// Asserts that exactly the `expected` checks fail.
        fn fails_with(&self, expected: &[&str]) {
            let r = self.run();
            let mut failed = r.failed();
            failed.dedup();
            assert_eq!(failed, expected, "{:#?}", r.findings);
        }
    }

    /// The reference isolation profile passes every check.
    #[test]
    fn reference_profile_passes() {
        let f = fixture();
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

    /// A group-writable config file fails `config-file`.
    #[test]
    fn writable_config_file_fails() {
        let f = fixture();
        mode(&f.opts.config, 0o664);
        f.fails_with(&["config-file"]);
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

    /// An env credential whose variable is unset fails `credentials`.
    #[test]
    fn missing_env_credential_fails() {
        let f = fixture();
        let text = std::fs::read_to_string(&f.opts.config).unwrap().replace(
            "[credentials]\nfile = \"credentials.json\"",
            "[[credentials.env]]\nid = \"business-api\"\nvar = \"CROM_GUARD_CHECK_TEST_UNSET\"\nactions = [\"refund.issue\"]",
        );
        std::fs::write(&f.opts.config, text).unwrap();
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
        let text = std::fs::read_to_string(&f.opts.config)
            .unwrap()
            .replace("unix:admin.sock", "127.0.0.1:8471");
        std::fs::write(&f.opts.config, text).unwrap();
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

    /// A sandbox read path covering Guard's state directory fails `sandbox-paths`.
    #[test]
    fn read_path_over_state_fails() {
        let f = fixture();
        let mut s = settings();
        s["sandbox"]["read_paths"] = json!([f.dir.path()]);
        f.set_settings(s);
        f.fails_with(&["sandbox-paths"]);
    }

    /// An agent workspace that contains Guard's state directory fails `sandbox-paths`.
    #[test]
    fn workspace_over_state_fails() {
        let mut f = fixture();
        f.opts.crom_workspace = Some(f.dir.path().to_path_buf());
        f.fails_with(&["sandbox-paths"]);
    }

    /// Passing Guard's admin-token variable into the sandbox fails `sandbox-env`.
    #[test]
    fn env_allow_of_guard_secret_fails() {
        let f = fixture();
        let text = std::fs::read_to_string(&f.opts.config).unwrap().replace(
            "token_file = \"admin.token\"",
            "token_env = \"CROM_GUARD_CHECK_TOKEN\"",
        );
        std::fs::write(&f.opts.config, text).unwrap();
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

    /// A stdio MCP server, even as the only one, fails `mcp-servers`.
    #[test]
    fn stdio_server_fails() {
        let f = fixture();
        let mut s = settings();
        s["mcp"]["servers"] = json!({"guard": {"type": "stdio", "command": "crom-guard"}});
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
