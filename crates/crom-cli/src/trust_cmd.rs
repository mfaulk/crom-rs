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
    if settings.workspace_trust.is_trusted() {
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
    out
}

/// Interactive only (stdin and stderr are terminals): ask whether to trust
/// `cwd`, persisting a yes. Headless callers never get here and keep the
/// restricted config.
pub fn prompt_for_trust(cwd: &Path, pending: &[String]) -> Result<bool> {
    let mut err = std::io::stderr();
    writeln!(
        err,
        "This workspace has repository config that would loosen permissions or start processes:"
    )?;
    for p in pending {
        writeln!(err, "  - {p}")?;
    }
    write!(
        err,
        "Trust {} and apply it? Saved for future runs; undo with `crom untrust`. [y/N] ",
        cwd.display()
    )?;
    err.flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
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
        .enumerate()
        .map(|(i, d)| d.to_security_decision(&session_id, &run_id, cwd, i))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
