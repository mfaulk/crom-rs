use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::events::{ValidationCommandResult, ValidationReport};

/// Maximum bytes captured per stream when running a validation command.
pub const VALIDATION_OUTPUT_CAP: usize = 64 * 1024;

/// Run validation commands serially. Returns a report whose `passed` is
/// true iff every command exited 0.
pub async fn run_validation(
    cwd: &Path,
    commands: &[String],
    per_command_timeout: Duration,
) -> ValidationReport {
    let mut results = Vec::with_capacity(commands.len());
    let mut all_passed = true;
    for cmd in commands {
        let r = run_one(cwd, cmd, per_command_timeout).await;
        if r.exit_code != Some(0) || r.timed_out {
            all_passed = false;
        }
        results.push(r);
    }
    ValidationReport {
        passed: all_passed && !commands.is_empty(),
        results,
    }
}

async fn run_one(cwd: &Path, command: &str, dur: Duration) -> ValidationCommandResult {
    let started = Instant::now();
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ValidationCommandResult {
                command: command.to_string(),
                exit_code: None,
                stdout_preview: String::new(),
                stderr_preview: format!("spawn failed: {e}"),
                duration_ms: 0,
                timed_out: false,
            };
        }
    };
    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    let stdout_task = tokio::spawn(async move { read_capped(&mut stdout).await });
    let stderr_task = tokio::spawn(async move { read_capped(&mut stderr).await });
    #[cfg(unix)]
    let pid = child.id();

    let wait_res = timeout(dur, child.wait()).await;
    let (exit_code, timed_out) = match wait_res {
        Ok(Ok(s)) => (s.code(), false),
        Ok(Err(_)) => (None, false),
        Err(_) => {
            #[cfg(unix)]
            if let Some(p) = pid {
                unsafe {
                    libc::kill(-(p as libc::pid_t), libc::SIGKILL);
                }
            } else {
                let _ = child.start_kill();
            }
            #[cfg(not(unix))]
            {
                let _ = child.start_kill();
            }
            let _ = child.wait().await;
            (None, true)
        }
    };

    let stdout_bytes = stdout_task.await.unwrap_or_default();
    let stderr_bytes = stderr_task.await.unwrap_or_default();
    let stdout_preview = String::from_utf8_lossy(&stdout_bytes).to_string();
    let stderr_preview = String::from_utf8_lossy(&stderr_bytes).to_string();

    ValidationCommandResult {
        command: command.to_string(),
        exit_code,
        stdout_preview,
        stderr_preview,
        duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        timed_out,
    }
}

async fn read_capped<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> Vec<u8> {
    let cap = VALIDATION_OUTPUT_CAP;
    let mut buf = Vec::with_capacity(cap.min(8192));
    let mut chunk = [0u8; 4096];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() < cap {
                    let take = (cap - buf.len()).min(n);
                    buf.extend_from_slice(&chunk[..take]);
                }
            }
            Err(_) => break,
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    // A passing command produces a passed report.
    async fn passing_command_passes() {
        let dir = TempDir::new().unwrap();
        let report =
            run_validation(dir.path(), &["true".to_string()], Duration::from_secs(5)).await;
        assert!(report.passed);
        assert_eq!(report.results[0].exit_code, Some(0));
    }

    #[tokio::test]
    // A failing command produces a failed report.
    async fn failing_command_fails() {
        let dir = TempDir::new().unwrap();
        let report =
            run_validation(dir.path(), &["false".to_string()], Duration::from_secs(5)).await;
        assert!(!report.passed);
        assert_eq!(report.results[0].exit_code, Some(1));
    }

    #[tokio::test]
    // A long-running command times out and is reported as such.
    async fn timeout_marks_timed_out() {
        let dir = TempDir::new().unwrap();
        let report = run_validation(
            dir.path(),
            &["sleep 3".to_string()],
            Duration::from_millis(150),
        )
        .await;
        assert!(!report.passed);
        assert!(report.results[0].timed_out);
    }
}
