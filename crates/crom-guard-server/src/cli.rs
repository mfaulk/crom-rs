//! The `crom-guard` command line. Hosts that compile in their own protected
//! operations call [`main_with`] from their own `main`.

use std::path::Path;
use std::process::ExitCode;

use crom_guard::{verify_file, AuditReceipt, CedarPdp, OperationRegistry};

use crate::config::{AdminListen, Config};

const USAGE: &str = "usage:
  crom-guard serve --config <guard.toml> [--agent-listen <addr>] [--admin-listen <addr|unix:path>]
  crom-guard policy validate --namespace <ns> --schema <file> --policies <file>
  crom-guard audit verify <path> [--anchor <seq>:<hash>]...";

/// Exit codes: 0 success; 1 refused to start, failed, or verification
/// failed; 2 usage error.
pub fn main_with(operations: impl FnOnce() -> OperationRegistry) -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => serve(&args[1..], operations),
        Some("policy") => policy(&args[1..]),
        Some("audit") => audit(&args[1..]),
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

/// `--flag value` pairs; `None` on a missing value, unknown flag, or repeat.
fn flags<'a>(args: &'a [String], known: &[&str]) -> Option<Vec<(&'a str, &'a str)>> {
    let mut out: Vec<(&str, &str)> = Vec::new();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let value = it.next()?;
        if !known.contains(&flag.as_str()) || out.iter().any(|(f, _)| f == flag) {
            return None;
        }
        out.push((flag, value));
    }
    Some(out)
}

fn serve(args: &[String], operations: impl FnOnce() -> OperationRegistry) -> ExitCode {
    let Some(flags) = flags(args, &["--config", "--agent-listen", "--admin-listen"]) else {
        return usage();
    };
    let Some(&(_, path)) = flags.iter().find(|(f, _)| *f == "--config") else {
        return usage();
    };
    init_tracing();
    let mut config = match Config::load(Path::new(path)) {
        Ok(c) => c,
        Err(e) => return refuse(&e),
    };
    for (flag, value) in &flags {
        match *flag {
            "--agent-listen" => match value.parse() {
                Ok(a) => config.agent.listen = a,
                Err(_) => return usage(),
            },
            "--admin-listen" => match value.parse::<AdminListen>() {
                Ok(a) => config.admin.listen = a,
                Err(_) => return usage(),
            },
            _ => {}
        }
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => return refuse(&e),
    };
    runtime.block_on(async move {
        let server = match crate::start(config, operations()).await {
            Ok(s) => s,
            Err(e) => return refuse(&e),
        };
        wait_for_signal().await;
        tracing::info!(
            in_flight = server.in_flight(),
            "shutting down; letting in-flight calls finish"
        );
        server.shutdown().await;
        ExitCode::SUCCESS
    })
}

fn refuse(e: &dyn std::fmt::Display) -> ExitCode {
    eprintln!("crom-guard: refusing to start: {e}");
    ExitCode::from(1)
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("CROM_GUARD_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,rmcp=warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

fn policy(args: &[String]) -> ExitCode {
    let Some(("validate", rest)) = args.split_first().map(|(c, r)| (c.as_str(), r)) else {
        return usage();
    };
    let Some(flags) = flags(rest, &["--namespace", "--schema", "--policies"]) else {
        return usage();
    };
    let get = |name| flags.iter().find(|(f, _)| *f == name).map(|(_, v)| *v);
    let (Some(ns), Some(schema), Some(policies)) =
        (get("--namespace"), get("--schema"), get("--policies"))
    else {
        return usage();
    };
    match CedarPdp::from_files(ns, Path::new(schema), Path::new(policies)) {
        Ok(_) => {
            println!("ok: policies validate against the schema");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("crom-guard: policy invalid: {e}");
            ExitCode::from(1)
        }
    }
}

/// Exit 0: chain intact. 1: verification failed (the `AuditChainBroken`
/// probe). 2: usage error.
fn audit(args: &[String]) -> ExitCode {
    let Some(("verify", rest)) = args.split_first().map(|(c, r)| (c.as_str(), r)) else {
        return usage();
    };
    let Some((path, mut flags)) = rest.split_first() else {
        return usage();
    };
    let mut anchors = Vec::new();
    while let Some((flag, tail)) = flags.split_first() {
        let anchor = match (flag.as_str(), tail.first()) {
            ("--anchor", Some(value)) => parse_anchor(value),
            _ => None,
        };
        let Some(anchor) = anchor else {
            return usage();
        };
        anchors.push(anchor);
        flags = &tail[1..];
    }
    match verify_file(path, &anchors) {
        Ok(summary) => {
            match summary.head {
                Some(h) => println!(
                    "ok: {} records; head seq {} hash {}",
                    summary.records, h.seq, h.head_hash
                ),
                None => println!("ok: 0 records"),
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("crom-guard: audit chain broken: {path}: {e}");
            ExitCode::from(1)
        }
    }
}

fn parse_anchor(s: &str) -> Option<AuditReceipt> {
    let (seq, hash) = s.split_once(':')?;
    let hex = hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !hex {
        return None;
    }
    Some(AuditReceipt {
        seq: seq.parse().ok()?,
        head_hash: hash.to_owned(),
    })
}
