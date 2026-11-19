use std::process::ExitCode;

/// The stock binary compiles in no protected operations; a deployment
/// builds its own binary that registers them and calls
/// `crom_guard_server::cli::main_with` (see `docs/guard/server.md`).
fn main() -> ExitCode {
    crom_guard_server::cli::main_with(crom_guard::OperationRegistry::new)
}
