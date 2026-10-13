use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::timeout;

use crom_core::{PermissionKind, Tool, ToolContext, ToolError, ToolResult};

use crate::limits::ToolLimits;
use crate::truncate_with_marker;

/// Names of environment variables we never expose in returned metadata.
const SECRET_ENV_PATTERNS: &[&str] = &[
    "KEY", "SECRET", "TOKEN", "PASSWORD", "PASSWD", "ACCESS", "PRIVATE",
];

/// Hard secondary deadline for draining stdout/stderr after we've decided the
/// child is finished or has been killed. If a descendant inherited the pipe
/// and refuses to release it, this prevents `execute()` from hanging forever.
const DRAIN_DEADLINE: Duration = Duration::from_secs(2);

/// Marker appended to the captured stream when the drain task had to be
/// aborted because a descendant kept the pipe open past `DRAIN_DEADLINE`.
const DRAIN_ABORTED_MARKER: &str = "\n[bash: drain aborted after kill]";

#[derive(Debug, Deserialize)]
struct Input {
    command: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

pub struct BashTool {
    limits: Arc<ToolLimits>,
}

impl BashTool {
    pub fn new(limits: Arc<ToolLimits>) -> Self {
        Self { limits }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        "Run a shell command from the workspace root. Output is capped, stdin is closed, and the command is killed after the configured timeout."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 600}
            },
            "required": ["command"]
        })
    }

    fn permission_kind(&self) -> PermissionKind {
        PermissionKind::Execution
    }

    fn approval_summary(&self, input: &serde_json::Value) -> String {
        let c = input.get("command").and_then(|v| v.as_str()).unwrap_or("?");
        format!("Bash: {c}")
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: Input = serde_json::from_value(input)
            .map_err(|e| ToolError::InvalidInput(format!("Bash input: {e}")))?;

        // Cap caller-supplied timeout to the configured maximum so that a
        // crafted tool input cannot exceed the operator-configured ceiling.
        let max_dur = self.limits.bash_timeout;
        let dur = parsed
            .timeout_secs
            .map(Duration::from_secs)
            .map(|d| d.min(max_dur))
            .unwrap_or(max_dur);
        let cap = self.limits.max_output_bytes;

        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg(&parsed.command)
            .current_dir(&ctx.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // Place the child in its own process group so that timeouts kill any
        // grandchild processes (e.g., builds, servers) the script started,
        // not just the bash interpreter itself.
        #[cfg(unix)]
        {
            // `process_group` is an inherent method on `std::process::Command`
            // (and re-exposed by tokio's wrapper) since Rust 1.64.
            cmd.process_group(0);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::ExecutionFailed(format!("spawn bash: {e}")))?;
        #[cfg(unix)]
        let child_pid: Option<u32> = child.id();

        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        // Share the partial buffer with the drain task so that if we have to
        // abort the task (a descendant inherited the pipe and won't release
        // it), we can still surface whatever was captured so far.
        let stdout_buf = Arc::new(AsyncMutex::new(Vec::<u8>::new()));
        let stderr_buf = Arc::new(AsyncMutex::new(Vec::<u8>::new()));

        let stdout_task = tokio::spawn(drain_into(stdout, stdout_buf.clone(), cap + 1));
        let stderr_task = tokio::spawn(drain_into(stderr, stderr_buf.clone(), cap + 1));

        let wait_res = timeout(dur, child.wait()).await;
        let timed_out;
        let status = match wait_res {
            Ok(Ok(s)) => {
                timed_out = false;
                Some(s)
            }
            Ok(Err(e)) => {
                return Err(ToolError::ExecutionFailed(format!("wait: {e}")));
            }
            Err(_) => {
                // Kill the entire process group so any grandchildren the
                // script forked are also reaped. Fall back to start_kill on
                // non-Unix platforms or if we never got a pid.
                #[cfg(unix)]
                if let Some(pid) = child_pid {
                    // SAFETY: kill(2) with a negative pid signals the whole
                    // process group; we set the group ourselves above. The
                    // returned int is just the syscall result, no Rust
                    // invariants are touched.
                    unsafe {
                        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                    }
                } else {
                    let _ = child.start_kill();
                }
                #[cfg(not(unix))]
                {
                    let _ = child.start_kill();
                }
                let _ = child.wait().await;
                timed_out = true;
                None
            }
        };

        // Bound the drain join with a hard deadline. Belt-and-suspenders for
        // descendants that inherited the pipes: on Unix the SIGKILL to the
        // process group usually causes the readers to hit EOF, but on
        // non-Unix only the bash process is killed and a forked child could
        // hold the pipe open indefinitely.
        let (stdout_bytes, stdout_aborted) =
            join_with_deadline(stdout_task, stdout_buf, DRAIN_DEADLINE)
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("stdout join: {e}")))?;
        let (stderr_bytes, stderr_aborted) =
            join_with_deadline(stderr_task, stderr_buf, DRAIN_DEADLINE)
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("stderr join: {e}")))?;

        let mut stdout_str = String::from_utf8_lossy(&stdout_bytes).into_owned();
        let mut stderr_str = String::from_utf8_lossy(&stderr_bytes).into_owned();
        if stdout_aborted {
            stdout_str.push_str(DRAIN_ABORTED_MARKER);
        }
        if stderr_aborted {
            stderr_str.push_str(DRAIN_ABORTED_MARKER);
        }
        let stdout_str = truncate_with_marker(&stdout_str, cap);
        let stderr_str = truncate_with_marker(&stderr_str, cap);

        let exit_code = status.as_ref().and_then(|s| s.code());
        let mut text = String::new();
        if !stdout_str.is_empty() {
            text.push_str("[stdout]\n");
            text.push_str(&stdout_str);
            text.push('\n');
        }
        if !stderr_str.is_empty() {
            text.push_str("[stderr]\n");
            text.push_str(&stderr_str);
            text.push('\n');
        }
        if timed_out {
            text.push_str(&format!("[killed: timeout after {}s]\n", dur.as_secs()));
        } else {
            text.push_str(&format!(
                "[exit: {}]\n",
                exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "?".into())
            ));
        }

        // `is_error` flows to the Anthropic tool_result block and signals
        // that the tool itself failed. A non-zero exit code is *output*, not
        // a tool failure — the model and CLI can read the `[exit: N]` line
        // and decide what it means in domain terms (e.g. failing tests are
        // exactly what the agent was asked to surface). Only flip the flag
        // when we couldn't run the command to a clean exit: timeouts, or a
        // signal-killed child where no exit code is available.
        let is_error = timed_out || exit_code.is_none();
        let metadata = json!({
            "exit_code": exit_code,
            "timed_out": timed_out,
            "command_redacted": redact(&parsed.command),
        });
        Ok(ToolResult {
            content: vec![crom_core::ToolContent::Text(text)],
            is_error,
            metadata,
        })
    }
}

/// Drain `r` into `buf`, capping the retained bytes at `cap`.
///
/// Continues reading after the cap is reached so the child process does not
/// block on a full pipe; the extra bytes are discarded. The shared `buf`
/// lets the parent task recover whatever was captured even if this task is
/// aborted (e.g. a descendant inherited the pipe and refuses to release it).
async fn drain_into<R>(mut r: R, buf: Arc<AsyncMutex<Vec<u8>>>, cap: usize)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut chunk = [0u8; 4096];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                let mut guard = buf.lock().await;
                if guard.len() < cap {
                    let take = (cap - guard.len()).min(n);
                    guard.extend_from_slice(&chunk[..take]);
                }
                // Continue draining so the child does not block on a full
                // pipe, but discard data once we've reached the cap.
            }
            Err(_) => break,
        }
    }
}

/// Wait for the drain task to finish, but no longer than `deadline`. If the
/// timeout fires, abort the task and return whatever bytes the task managed
/// to capture into the shared buffer before being torn down.
///
/// Returns `Ok((bytes, aborted))` where `aborted` is true when the deadline
/// fired before the task completed. Aborting the task drops the underlying
/// `ChildStdout`/`ChildStderr` reader the task owned, which closes the read
/// side of the pipe even if a descendant of the killed child is still
/// holding the write side open. Tokio's child stdout/stderr types are
/// `Drop`-safe while a reader future is in flight: the task simply stops
/// being polled and the resource is released cleanly.
///
/// A `JoinError` from a *panicked* drain task is surfaced as `Err` so the
/// caller does not silently report a successful execution while a drain
/// panic was swallowed. Cancellation errors from our own `abort()` after a
/// deadline are expected and are reported as `aborted = true`, not as an
/// error.
async fn join_with_deadline(
    mut task: tokio::task::JoinHandle<()>,
    buf: Arc<AsyncMutex<Vec<u8>>>,
    deadline: Duration,
) -> Result<(Vec<u8>, bool), tokio::task::JoinError> {
    let abort = task.abort_handle();
    let aborted = match timeout(deadline, &mut task).await {
        Ok(Ok(())) => false,
        Ok(Err(join_err)) => {
            // The task finished before the deadline but did not return
            // cleanly. A cancellation here is unexpected (we have not
            // aborted yet), so treat anything non-clean as an error to
            // propagate.
            return Err(join_err);
        }
        Err(_elapsed) => {
            // Tear the drain task down so it drops the pipe reader, then
            // await the handle to observe the cancellation and avoid
            // leaving an orphaned task running in the background.
            abort.abort();
            match task.await {
                Ok(()) => {}
                Err(join_err) if join_err.is_cancelled() => {
                    // Expected: we just aborted it.
                }
                Err(join_err) => return Err(join_err),
            }
            true
        }
    };
    let bytes = std::mem::take(&mut *buf.lock().await);
    Ok((bytes, aborted))
}

/// Scrub secrets from a shell command before it is recorded as audit /
/// telemetry metadata.
///
/// Composition (in order):
/// 1. Conservative env-var-name match — if the command text contains a
///    needle like `KEY`/`SECRET`/`TOKEN`, replace the whole command with
///    a marker. This catches commands shaped like
///    `export FOO_TOKEN=...` where the value is in a custom format the
///    token-shape pass cannot recognize.
/// 2. Token-shape redaction (`redact_secrets_in_str`) — surgically
///    replaces well-known secret formats (Anthropic / OpenAI / Google API
///    keys, AWS access keys, GitHub tokens, `Bearer ...` headers) with
///    `[REDACTED]`, leaving the rest of the command intact.
///
/// Both passes are needed: env-var-name matching by itself was bypassed
/// by `curl -H 'Authorization: Bearer sk-…'`-style commands (no needle in
/// the text) and was the regression tracked in #142.
fn redact(cmd: &str) -> String {
    let lower = cmd.to_ascii_lowercase();
    for needle in SECRET_ENV_PATTERNS {
        let n = needle.to_ascii_lowercase();
        if lower.contains(&n) {
            return format!("<redacted: contains pattern {n}>");
        }
    }
    crom_core::builtin_hooks::redact_secrets_in_str(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, ReadBuf};

    /// A reader that delivers `prefix` once and then never returns either
    /// EOF or further data: subsequent polls always report Pending. This
    /// simulates a descendant of the killed child that inherited the pipe
    /// and refuses to release it.
    struct NeverEofReader {
        prefix: Vec<u8>,
        sent: bool,
    }

    impl AsyncRead for NeverEofReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if !self.sent {
                let n = self.prefix.len().min(buf.remaining());
                buf.put_slice(&self.prefix[..n]);
                self.sent = true;
                Poll::Ready(Ok(()))
            } else {
                // Never wake the waker: this future is intentionally stuck
                // until the parent aborts the task.
                Poll::Pending
            }
        }
    }

    // join_with_deadline aborts a drain task that is stuck on a never-EOF
    // reader and returns the partial buffer plus an aborted flag, instead
    // of hanging forever (the bug from issue #12).
    #[tokio::test]
    async fn drain_with_deadline_aborts_on_never_eof() {
        let reader = NeverEofReader {
            prefix: b"partial".to_vec(),
            sent: false,
        };
        let buf = Arc::new(AsyncMutex::new(Vec::<u8>::new()));
        let task = tokio::spawn(drain_into(reader, buf.clone(), 1024));

        let started = std::time::Instant::now();
        let (bytes, aborted) = join_with_deadline(task, buf, Duration::from_millis(150))
            .await
            .expect("aborted-by-deadline must not be a join error");
        let elapsed = started.elapsed();

        assert!(aborted, "expected drain to be aborted by deadline");
        assert_eq!(bytes, b"partial");
        assert!(
            elapsed < Duration::from_secs(1),
            "deadline must fire promptly, took {elapsed:?}"
        );
    }

    // join_with_deadline returns the full buffer with aborted=false when
    // the reader reaches EOF before the deadline.
    #[tokio::test]
    async fn drain_with_deadline_completes_on_eof() {
        let reader: &[u8] = b"hello";
        let buf = Arc::new(AsyncMutex::new(Vec::<u8>::new()));
        let task = tokio::spawn(drain_into(reader, buf.clone(), 1024));

        let (bytes, aborted) = join_with_deadline(task, buf, Duration::from_secs(5))
            .await
            .expect("clean EOF must not be a join error");

        assert!(!aborted);
        assert_eq!(bytes, b"hello");
    }

    // join_with_deadline propagates a panic in the drain task as a JoinError
    // instead of silently reporting success. This guards against the
    // regression where Ok(_join) discarded the JoinHandle result.
    #[tokio::test]
    async fn drain_with_deadline_propagates_panic() {
        let buf = Arc::new(AsyncMutex::new(Vec::<u8>::new()));
        // Spawn a task whose Future return type matches drain_into's `()`
        // and panics shortly after start, before the deadline fires.
        let task: tokio::task::JoinHandle<()> = tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            panic!("drain task panic");
        });

        let result = join_with_deadline(task, buf, Duration::from_secs(5)).await;

        let err = result.expect_err("panicked drain task must surface JoinError");
        assert!(
            err.is_panic(),
            "expected panic-flavored JoinError, got {err:?}"
        );
    }

    // redact() must scrub a Bearer-token Authorization header even though
    // the command text contains none of the SECRET_ENV_PATTERNS needles.
    // Regression for issue #142.
    #[test]
    fn redact_scrubs_bearer_authorization_header() {
        let cmd = "curl -H 'Authorization: Bearer sk-ant-api03-aaaaaaaaaaaaaaaaaa' https://api.example.com";
        let out = redact(cmd);
        assert!(
            !out.contains("sk-ant-api03"),
            "raw Anthropic key leaked: {out}"
        );
        assert!(out.contains("[REDACTED]"), "expected [REDACTED] in: {out}");
    }

    // redact() must scrub an AWS access key id embedded in a URL even
    // when the surrounding text does not match any env-var-name needle.
    #[test]
    fn redact_scrubs_aws_access_key_in_url() {
        let cmd =
            "aws s3 cp foo s3://bucket/ --endpoint-url https://AKIAIOSFODNN7EXAMPLE.example.com";
        let out = redact(cmd);
        assert!(
            !out.contains("AKIAIOSFODNN7EXAMPLE"),
            "AWS key leaked: {out}"
        );
        assert!(out.contains("[REDACTED]"));
    }

    // redact() must scrub a GitHub personal access token passed via a
    // bare `Authorization: <token>` header. This command happens to hit
    // the env-var-name fallback (substring "token") before the
    // token-shape pass, so the entire command is replaced with the
    // marker — either form is acceptable as long as the secret value is
    // gone.
    #[test]
    fn redact_scrubs_github_personal_access_token() {
        let cmd = "gh api -H 'Authorization: token ghp_abcdefghijklmnopqrstuvwxyz0123' /user";
        let out = redact(cmd);
        assert!(
            !out.contains("ghp_abcdefghijklmnopqrstuvwxyz0123"),
            "GitHub token leaked: {out}"
        );
        assert!(
            out.contains("[REDACTED]") || out.starts_with("<redacted:"),
            "expected redaction marker in: {out}"
        );
    }

    // redact() must scrub a GitHub token embedded in a URL where no
    // env-var-name needle (KEY/SECRET/TOKEN/...) appears anywhere in
    // the command, so only the token-shape pass can save us.
    #[test]
    fn redact_scrubs_github_token_with_no_needle_in_text() {
        let cmd = "curl https://ghp_abcdefghijklmnopqrstuvwxyz0123@example.com/repo.git";
        let out = redact(cmd);
        assert!(!out.contains("ghp_abcdefghijklmnopqrstuvwxyz0123"));
        assert!(out.contains("[REDACTED]"), "expected [REDACTED] in: {out}");
    }

    // redact() preserves the existing conservative env-var-name fallback:
    // a command that contains the substring `KEY` is replaced wholesale,
    // matching the long-standing behavior.
    #[test]
    fn redact_env_var_name_fallback_replaces_entire_command() {
        let out = redact("export FOO_API_KEY=mysecret");
        assert!(out.starts_with("<redacted: contains pattern"));
    }

    // redact() leaves commands with no recognizable secret material
    // unchanged.
    #[test]
    fn redact_passes_through_benign_command() {
        let cmd = "ls -la /tmp";
        assert_eq!(redact(cmd), cmd);
    }
}
