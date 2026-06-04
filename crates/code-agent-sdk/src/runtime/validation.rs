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
    let child = match Command::new("sh")
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

#[cfg(test)]
mod tests {
    //! Branch coverage for `run_validation_command`: the timeout arm (driven
    //! deterministically with paused tokio time rather than a real sleep),
    //! the cancellation arm, the success path, and the output-cap helper.

    use super::*;
    use std::time::Duration;

    fn policy(timeout: Duration) -> ValidationPolicy {
        ValidationPolicy {
            commands: Vec::new(),
            run_after_completion: true,
            require_success: false,
            timeout,
            output_limit_bytes: 64 * 1024,
        }
    }

    // The timeout arm: a command that sleeps far longer than the policy
    // timeout must be reported as `timed_out: true` / `success: false` once
    // the `tokio::time::sleep(policy.timeout)` select arm fires. A tiny real
    // timeout against a 30s `sleep` makes the race deterministic without
    // relying on tokio's `test-util` paused clock (not enabled for this
    // crate's dev-deps).
    #[tokio::test]
    async fn timeout_branch_marks_timed_out() {
        let cwd = std::env::temp_dir();
        let cancel = CancellationHandle::new();
        let report =
            run_validation_command(&cwd, "sleep 30", &policy(Duration::from_millis(20)), cancel)
                .await;
        assert!(report.timed_out, "expected timed_out report");
        assert!(!report.success);
        assert_eq!(report.exit_code, None);
    }

    // The cancellation arm: cancelling before/while the command runs skips it
    // (reported via `ValidationReport::skipped`, i.e. not successful and not
    // timed out). The timeout is large so only the cancel arm can fire.
    #[tokio::test]
    async fn cancellation_branch_skips_command() {
        let cwd = std::env::temp_dir();
        let cancel = CancellationHandle::new();
        // Cancel up front; the long-sleeping command must be skipped via the
        // `cancellation.cancelled()` arm rather than completing or timing out.
        cancel.cancel();
        let report =
            run_validation_command(&cwd, "sleep 30", &policy(Duration::from_secs(600)), cancel)
                .await;
        assert!(!report.success);
        assert!(!report.timed_out, "cancel is not a timeout");
    }

    // Happy path: a quick successful command reports success with exit code 0
    // and is not timed out.
    #[tokio::test]
    async fn success_branch_reports_exit_code() {
        let cwd = std::env::temp_dir();
        let cancel = CancellationHandle::new();
        let report = run_validation_command(
            &cwd,
            "printf hello",
            &policy(Duration::from_secs(30)),
            cancel,
        )
        .await;
        assert!(report.success);
        assert_eq!(report.exit_code, Some(0));
        assert!(!report.timed_out);
        assert!(report.stdout.contains("hello"));
    }

    // A non-zero exit reports failure (but not a timeout).
    #[tokio::test]
    async fn nonzero_exit_reports_failure() {
        let cwd = std::env::temp_dir();
        let cancel = CancellationHandle::new();
        let report =
            run_validation_command(&cwd, "exit 3", &policy(Duration::from_secs(30)), cancel).await;
        assert!(!report.success);
        assert_eq!(report.exit_code, Some(3));
        assert!(!report.timed_out);
    }

    // truncate_bytes caps oversized output and passes short output through.
    #[test]
    fn truncate_bytes_caps_and_passes_through() {
        assert_eq!(truncate_bytes(b"hello", 3), "hel");
        assert_eq!(truncate_bytes(b"hi", 8), "hi");
        assert_eq!(truncate_bytes(b"", 4), "");
    }
}
