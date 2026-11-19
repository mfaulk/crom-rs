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

/// The `tracing` filter: `CROM_GUARD_LOG` (default `info`) with every
/// `rmcp` directive replaced by `rmcp=<CROM_GUARD_RMCP_LOG or warn>`.
/// rmcp at `debug` logs whole JSON-RPC messages (tool arguments, and any
/// secret an operation returns), so raising it takes a separate, explicit
/// variable.
pub fn log_filter(guard_log: Option<&str>, rmcp_log: Option<&str>) -> String {
    let mut directives: Vec<&str> = guard_log
        .unwrap_or("info")
        .split(',')
        .map(str::trim)
        .filter(|d| {
            let target = d.split(['=', '[']).next().unwrap_or("");
            !d.is_empty() && target != "rmcp" && !target.starts_with("rmcp::")
        })
        .collect();
    let rmcp = format!("rmcp={}", rmcp_log.unwrap_or("warn"));
    directives.push(&rmcp);
    directives.join(",")
}

fn init_tracing() {
    let guard = std::env::var("CROM_GUARD_LOG").ok();
    let rmcp = std::env::var("CROM_GUARD_RMCP_LOG").ok();
    let spec = log_filter(guard.as_deref(), rmcp.as_deref());
    let filter = tracing_subscriber::EnvFilter::try_new(&spec).unwrap_or_else(|e| {
        eprintln!("crom-guard: invalid log filter ({e}); using info,rmcp=warn");
        tracing_subscriber::EnvFilter::new("info,rmcp=warn")
    });
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

#[cfg(test)]
mod tests {
    use super::log_filter;

    /// rmcp is pinned to `warn` whatever `CROM_GUARD_LOG` says, including explicit rmcp directives.
    #[test]
    fn rmcp_stays_at_warn_unless_explicit() {
        assert_eq!(log_filter(None, None), "info,rmcp=warn");
        assert_eq!(log_filter(Some("debug"), None), "debug,rmcp=warn");
        assert_eq!(
            log_filter(
                Some("trace,rmcp=trace,rmcp::service=debug,crom_guard=debug"),
                None
            ),
            "trace,crom_guard=debug,rmcp=warn"
        );
        assert_eq!(log_filter(Some("debug"), Some("debug")), "debug,rmcp=debug");
    }

    /// The pinned filter actually disables rmcp debug events.
    #[test]
    fn pinned_filter_disables_rmcp_debug() {
        use tracing::subscriber::with_default;
        let filter = tracing_subscriber::EnvFilter::new(log_filter(Some("trace"), None));
        let sub = tracing_subscriber::fmt().with_env_filter(filter).finish();
        with_default(sub, || {
            assert!(!tracing::enabled!(
                target: "rmcp::service",
                tracing::Level::DEBUG
            ));
            assert!(tracing::enabled!(
                target: "rmcp::service",
                tracing::Level::WARN
            ));
            assert!(tracing::enabled!(
                target: "crom_guard",
                tracing::Level::TRACE
            ));
        });
    }
}
