//! `crom trust` / `crom untrust` and the startup trust gate for repo-local
//! configuration (#290, threat model P9).

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use crom_core::events::SecurityDecision;
use crom_core::trust::{self, UntrustedConfigDenial, WorkspaceTrust};
use crom_core::{RunId, SessionId, Settings};

#[derive(Parser, Debug)]
pub struct TrustArgs {
    /// Workspace to trust.
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// List trusted workspaces instead.
    #[arg(long, conflicts_with = "status")]
    pub list: bool,
    /// Print whether the workspace is trusted, without changing anything.
    #[arg(long)]
    pub status: bool,
}

#[derive(Parser, Debug)]
pub struct UntrustArgs {
    /// Workspace to stop trusting.
    #[arg(default_value = ".")]
    pub path: PathBuf,
}

pub fn cmd_trust(args: TrustArgs) -> Result<()> {
    if args.list {
        for p in trust::trusted_workspaces()? {
            println!("{}", p.display());
        }
        return Ok(());
    }
    if args.status {
        let t = trust::workspace_trust(&args.path);
        println!(
            "{}\t{}",
            if t.is_trusted() {
                "trusted"
            } else {
                "untrusted"
            },
            args.path
                .canonicalize()
                .unwrap_or_else(|_| args.path.clone())
                .display()
        );
        return Ok(());
    }
    let (ws, changed) = trust::set_workspace_trust(&args.path, WorkspaceTrust::Trusted)?;
    println!(
        "{} {}",
        if changed {
            "trusted"
        } else {
            "already trusted"
        },
        ws.display()
    );
    Ok(())
}

pub fn cmd_untrust(args: UntrustArgs) -> Result<()> {
    let (ws, changed) = trust::set_workspace_trust(&args.path, WorkspaceTrust::Untrusted)?;
    println!(
        "{} {}",
        if changed {
            "untrusted"
        } else {
            "was not trusted"
        },
        ws.display()
    );
    Ok(())
}

/// Repo-local config an untrusted workspace would have applied: denied
/// settings keys plus `.agent/mcp.json` when present.
pub fn pending_repo_config(cwd: &Path, settings: &Settings) -> Vec<String> {
    if settings.workspace_trust.is_trusted() || trust::workspace_trust(cwd).is_trusted() {
        return Vec::new();
    }
    let mut out: Vec<String> = settings
        .untrusted_denials
        .iter()
        .map(|d| format!(".agent/settings.json: {}", d.key))
        .collect();
    if cwd.join(".agent").join("mcp.json").exists() {
        out.push(".agent/mcp.json: MCP servers".into());
    }
    for d in crom_core::untrusted_project_agent_denials(cwd) {
        let file = d
            .source
            .strip_prefix(cwd)
            .unwrap_or(&d.source)
            .display()
            .to_string();
        out.push(format!("{file}: subagent definition"));
    }
    out
}

/// Project subagent definitions skipped because the workspace is untrusted.
fn project_agent_denials(cwd: &Path, settings: &Settings) -> Vec<UntrustedConfigDenial> {
    if settings.workspace_trust.is_trusted() || trust::workspace_trust(cwd).is_trusted() {
        Vec::new()
    } else {
        crom_core::untrusted_project_agent_denials(cwd)
    }
}

/// Interactive only (stdin and stderr are terminals): ask whether to trust
/// `cwd`, persisting a yes. Headless callers never get here and keep the
/// restricted config.
pub fn prompt_for_trust(cwd: &Path, pending: &[String]) -> Result<bool> {
    ask_trust(
        cwd,
        pending,
        &mut std::io::stdin().lock(),
        &mut std::io::stderr(),
    )
}

fn ask_trust(
    cwd: &Path,
    pending: &[String],
    input: &mut impl BufRead,
    out: &mut impl Write,
) -> Result<bool> {
    writeln!(
        out,
        "This workspace has repository config that would loosen permissions or start processes:"
    )?;
    for p in pending {
        writeln!(out, "  - {p}")?;
    }
    write!(
        out,
        "Trust {} and apply it? Saved for future runs; undo with `crom untrust`. [y/N] ",
        cwd.display()
    )?;
    out.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    if matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        trust::set_workspace_trust(cwd, WorkspaceTrust::Trusted)
            .context("persist workspace trust")?;
        return Ok(true);
    }
    Ok(false)
}

pub fn is_interactive_terminal() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// One blocked `SecurityDecision` per ignored repo-config entry.
pub fn trust_denial_decisions(
    cwd: &Path,
    settings: &Settings,
    mcp: &crom_mcp::McpOptions,
) -> Vec<SecurityDecision> {
    let session_id = SessionId::default();
    let run_id = RunId::new();
    let mcp_denials = mcp
        .repo_config_denials
        .iter()
        .map(|d| UntrustedConfigDenial::new(&d.source, d.key.clone(), d.reason.clone()));
    settings
        .untrusted_denials
        .iter()
        .cloned()
        .chain(mcp_denials)
        .chain(project_agent_denials(cwd, settings))
        .enumerate()
        .map(|(i, d)| d.to_security_decision(&session_id, &run_id, cwd, i))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Answering yes persists trust; anything else (including EOF) leaves the workspace untrusted.
    fn prompt_persists_only_on_yes() {
        let home = tempfile::TempDir::new().unwrap();
        let ws = tempfile::TempDir::new().unwrap();
        temp_env::with_vars(
            [
                ("CROM_CONFIG_HOME", Some(home.path().to_str().unwrap())),
                ("CODE_AGENT_CONFIG_HOME", None),
            ],
            || {
                let pending = vec![".agent/settings.json: allowed_tools".to_string()];
                for answer in ["", "n\n", "nope\n"] {
                    let mut out = Vec::new();
                    let trusted =
                        ask_trust(ws.path(), &pending, &mut answer.as_bytes(), &mut out).unwrap();
                    assert!(!trusted);
                    assert!(!trust::workspace_trust(ws.path()).is_trusted());
                    assert!(String::from_utf8(out).unwrap().contains("allowed_tools"));
                }
                let trusted =
                    ask_trust(ws.path(), &pending, &mut "Y\n".as_bytes(), &mut Vec::new()).unwrap();
                assert!(trusted);
                assert!(trust::workspace_trust(ws.path()).is_trusted());
            },
        );
    }

    #[test]
    // Repo subagent definitions are pending config and audited denials until the workspace is trusted.
    fn project_agents_are_denied_until_trusted() {
        let home = tempfile::TempDir::new().unwrap();
        let ws = tempfile::TempDir::new().unwrap();
        let cwd = ws.path().canonicalize().unwrap();
        std::fs::create_dir_all(cwd.join(".agent/agents")).unwrap();
        std::fs::write(cwd.join(".agent/agents/evil.md"), "x").unwrap();
        std::fs::write(cwd.join(".agent/agents/notes.txt"), "x").unwrap();
        temp_env::with_vars(
            [
                ("CROM_CONFIG_HOME", Some(home.path().to_str().unwrap())),
                ("CODE_AGENT_CONFIG_HOME", None),
            ],
            || {
                let s = Settings::default();
                let mcp = crom_mcp::McpOptions::default();
                assert_eq!(
                    pending_repo_config(&cwd, &s),
                    vec![".agent/agents/evil.md: subagent definition".to_string()]
                );
                let ds = trust_denial_decisions(&cwd, &s, &mcp);
                assert_eq!(ds.len(), 1);
                assert_eq!(
                    ds[0].resource.as_deref(),
                    Some(".agent/agents/evil.md#subagent_definition")
                );
                trust::set_workspace_trust(&cwd, WorkspaceTrust::Trusted).unwrap();
                assert!(pending_repo_config(&cwd, &s).is_empty());
                assert!(trust_denial_decisions(&cwd, &s, &mcp).is_empty());
            },
        );
    }

    #[test]
    // Each settings and MCP denial becomes a blocked workspace_trust SecurityDecision.
    fn denials_become_blocked_security_decisions() {
        let cwd = Path::new("/ws");
        let settings = Settings {
            untrusted_denials: vec![UntrustedConfigDenial::new(
                Path::new("/ws/.agent/settings.json"),
                "allowed_tools",
                "no",
            )],
            ..Settings::default()
        };
        let mcp = crom_mcp::McpOptions {
            repo_config_denials: vec![crom_mcp::McpRepoConfigDenial {
                source: PathBuf::from("/ws/.agent/mcp.json"),
                key: "mcpServers.evil".into(),
                reason: "no".into(),
            }],
            ..Default::default()
        };
        let ds = trust_denial_decisions(cwd, &settings, &mcp);
        assert_eq!(ds.len(), 2);
        assert_eq!(
            ds[0].resource.as_deref(),
            Some(".agent/settings.json#allowed_tools")
        );
        assert_eq!(
            ds[1].resource.as_deref(),
            Some(".agent/mcp.json#mcpServers.evil")
        );
        for d in &ds {
            assert_eq!(
                d.policy_source,
                Some(crom_core::PolicyDecisionSource::WorkspaceTrust)
            );
            assert_eq!(d.outcome, crom_core::events::SecurityOutcome::Blocked);
            assert_eq!(d.decision, crom_core::PolicyOutcome::Deny);
        }
    }
}
