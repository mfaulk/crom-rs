//! `crom-guard` with the support reference operations compiled in (MILESTONE_18
//! §14). Same CLI as the stock binary. Under `serve` it also hosts the testkit
//! business API in process, because the `support_ops` resolvers read its data
//! directly (the application's trusted data access).
//!
//! Environment, read only by `serve`:
//! - `CROM_GUARD_SUPPORT_CREDENTIALS` (required): the broker credentials file
//!   named by `guard.toml`; the business API accepts exactly these secrets.
//! - `CROM_GUARD_SUPPORT_API_LISTEN` (default `127.0.0.1:8480`).
//! - `CROM_GUARD_SUPPORT_LEDGER` (optional): JSONL ledger path, truncated at
//!   start; in memory when unset.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;

use crom_guard::OperationRegistry;
use crom_guard_testkit::{support_registry, ApiCredential, BusinessApi, Ledger, SupportConfig};

fn main() -> ExitCode {
    crom_guard_server::cli::main_with(|| match registry() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("crom-guard-support: refusing to start: {e}");
            std::process::exit(1);
        }
    })
}

fn registry() -> Result<OperationRegistry, String> {
    let creds_path = std::env::var_os("CROM_GUARD_SUPPORT_CREDENTIALS")
        .map(PathBuf::from)
        .ok_or("CROM_GUARD_SUPPORT_CREDENTIALS is not set")?;
    let text = std::fs::read_to_string(&creds_path)
        .map_err(|e| format!("reading {}: {e}", creds_path.display()))?;
    let credentials = ApiCredential::from_broker_json(&text)?;
    let listen: SocketAddr = std::env::var("CROM_GUARD_SUPPORT_API_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:8480".into())
        .parse()
        .map_err(|e| format!("CROM_GUARD_SUPPORT_API_LISTEN: {e}"))?;
    if !listen.ip().is_loopback() {
        return Err("CROM_GUARD_SUPPORT_API_LISTEN must be a loopback address".into());
    }
    let ledger = match std::env::var_os("CROM_GUARD_SUPPORT_LEDGER") {
        Some(p) => Ledger::with_file(&PathBuf::from(p)).map_err(|e| e.to_string())?,
        None => Ledger::in_memory(),
    };
    let api = BusinessApi::new(credentials, ledger);

    // main_with calls this inside its runtime, so the API gets its own
    // thread and runtime instead of a nested block_on.
    let (tx, rx) = mpsc::channel();
    let served = api.clone();
    std::thread::Builder::new()
        .name("business-api".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(e.to_string()));
                    return;
                }
            };
            runtime.block_on(async move {
                match served.serve(listen).await {
                    Ok(running) => {
                        let _ = tx.send(Ok(running.base_url()));
                        // Serve until the process exits.
                        std::future::pending::<()>().await;
                        drop(running);
                    }
                    Err(e) => {
                        let _ = tx.send(Err(format!("business API on {listen}: {e}")));
                    }
                }
            });
        })
        .map_err(|e| e.to_string())?;
    let base_url = rx
        .recv()
        .map_err(|_| "business API thread exited".to_string())??;
    eprintln!("crom-guard-support: business API at {base_url}");
    support_registry(&api, &SupportConfig::new(base_url)).map_err(|e| e.to_string())
}
