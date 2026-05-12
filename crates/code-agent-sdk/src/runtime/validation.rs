//! Validation command runner used after a successful agent run.

use std::path::Path;
use std::time::Instant;
use tokio::process::Command;

use crate::budget::CancellationHandle;
use crate::request::ValidationPolicy;
use crate::result::ValidationReport;

/// Run a validation command in `cwd`, observing the timeout and output cap
/// from `policy`. Respects cancellation by killing the spawned process.
pub(crate) async fn run_validation_command(
    cwd: &Path,
    command: &str,
    policy: &ValidationPolicy,
    cancellation: CancellationHandle,
) -> ValidationReport {
    let started = Instant::now();
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return ValidationReport::skipped(command, format!("spawn failed: {e}"));
        }
    };

    let wait = async {
        let output = child.wait_with_output().await;
        output
    };
    let result = tokio::select! {
        out = wait => out,
        _ = tokio::time::sleep(policy.timeout) => {
            return ValidationReport {
                command: command.to_string(),
                success: false,
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                timed_out: true,
            };
        }
        _ = cancellation.cancelled() => {
            return ValidationReport::skipped(command, "cancelled");
        }
    };
    let output = match result {
        Ok(o) => o,
        Err(e) => return ValidationReport::skipped(command, format!("wait failed: {e}")),
    };
    let cap = policy.output_limit_bytes;
    let stdout = truncate_bytes(&output.stdout, cap);
    let stderr = truncate_bytes(&output.stderr, cap);
    ValidationReport {
        command: command.to_string(),
        success: output.status.success(),
        exit_code: output.status.code(),
        stdout,
        stderr,
        duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        timed_out: false,
    }
}

fn truncate_bytes(bytes: &[u8], cap: usize) -> String {
    let slice = if bytes.len() > cap {
        &bytes[..cap]
    } else {
        bytes
    };
    String::from_utf8_lossy(slice).into_owned()
}
