//! `crom diagnostics` — parse captured command output into structured
//! diagnostics without an agent session (M16N).
//!
//! These operate on a captured stdout/stderr log file (e.g. the output of
//! `cargo test 2>&1 > log.txt`), so they are useful for debugging the parser
//! and for scripting.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crom_core::diagnostics::{
    compact, parse_command_diagnostic, CommandDiagnosticInput, CompactionConfig,
};

#[derive(Subcommand, Debug)]
pub enum DiagnosticsCommand {
    /// Parse a log file into the full structured diagnostic (JSON).
    Parse(DiagArgs),
    /// Print a compact, model-facing summary of a log file.
    Summarize(DiagArgs),
    /// Print the stable failure fingerprint of a log file.
    Fingerprint(DiagArgs),
}

#[derive(Parser, Debug)]
pub struct DiagArgs {
    /// Path to a captured stdout/stderr log file (`-` for stdin).
    pub log_file: String,
    /// The command that produced the log (used for classification + fingerprint).
    #[arg(long, default_value = "cargo test")]
    pub command: String,
    /// Exit code of the command, if known.
    #[arg(long)]
    pub exit_code: Option<i32>,
    /// Treat the run as having timed out.
    #[arg(long)]
    pub timed_out: bool,
    /// Working directory the command ran in (for path relativization).
    #[arg(long)]
    pub cwd: Option<PathBuf>,
}

pub fn run(cmd: DiagnosticsCommand) -> Result<()> {
    match cmd {
        DiagnosticsCommand::Parse(args) => {
            let diag = parse_args(&args)?;
            println!("{}", serde_json::to_string_pretty(&diag)?);
        }
        DiagnosticsCommand::Summarize(args) => {
            let diag = parse_args(&args)?;
            println!("{}", compact(&diag, &CompactionConfig::default()));
        }
        DiagnosticsCommand::Fingerprint(args) => {
            let diag = parse_args(&args)?;
            println!("kind:        {}", diag.summary.primary_kind.as_str());
            if let Some(code) = &diag.fingerprint.primary_code {
                println!("code:        {code}");
            }
            if let Some(f) = &diag.fingerprint.primary_file {
                println!("file:        {}", f.display());
            }
            println!("fingerprint: {}", diag.fingerprint.short_hash());
        }
    }
    Ok(())
}

fn parse_args(args: &DiagArgs) -> Result<crom_core::diagnostics::CommandDiagnostic> {
    let content = if args.log_file == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading stdin")?;
        buf
    } else {
        std::fs::read_to_string(&args.log_file)
            .with_context(|| format!("reading log file {}", args.log_file))?
    };
    let cwd = args
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    // The captured stream is treated as stderr (where compilers/tests print
    // diagnostics); the parser scans both streams regardless.
    // Default the exit code only for the non-timeout case (a timeout has no
    // meaningful exit code; leave it `None` rather than fabricating `0`).
    let exit_code = match (args.exit_code, args.timed_out) {
        (Some(c), _) => Some(c),
        (None, true) => None,
        (None, false) => Some(101),
    };
    Ok(parse_command_diagnostic(CommandDiagnosticInput {
        command: args.command.clone(),
        cwd,
        exit_code,
        timed_out: args.timed_out,
        stdout: String::new(),
        stderr: content,
        duration_ms: None,
    }))
}
