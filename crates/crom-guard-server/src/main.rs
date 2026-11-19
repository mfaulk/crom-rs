//! `crom-guard` service binary. The agent endpoint and admin API arrive in
//! #357; until then only `audit verify` runs, and starting the service
//! refuses rather than run without enforcement.

use std::process::ExitCode;

use crom_guard::{verify_file, AuditReceipt};

const USAGE: &str = "usage: crom-guard audit verify <path> [--anchor <seq>:<hash>]...";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("audit") => audit(&args[1..]),
        _ => {
            eprintln!("crom-guard: no agent endpoint is implemented yet (see #357)");
            eprintln!("{USAGE}");
            ExitCode::from(2)
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

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}
