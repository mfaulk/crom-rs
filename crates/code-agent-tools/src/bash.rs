use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

use code_agent_core::{PermissionKind, Tool, ToolContext, ToolError, ToolResult};

use crate::limits::ToolLimits;
use crate::truncate_with_marker;

/// Names of environment variables we never expose in returned metadata.
const SECRET_ENV_PATTERNS: &[&str] = &[
    "KEY", "SECRET", "TOKEN", "PASSWORD", "PASSWD", "ACCESS", "PRIVATE",
];

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

        let mut stdout = child.stdout.take().expect("stdout was piped");
        let mut stderr = child.stderr.take().expect("stderr was piped");

        let stdout_task = tokio::spawn(async move { read_capped(&mut stdout, cap + 1).await });
        let stderr_task = tokio::spawn(async move { read_capped(&mut stderr, cap + 1).await });

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

        let stdout_bytes = stdout_task
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("stdout join: {e}")))?;
        let stderr_bytes = stderr_task
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("stderr join: {e}")))?;

        let stdout_str = String::from_utf8_lossy(&stdout_bytes).into_owned();
        let stderr_str = String::from_utf8_lossy(&stderr_bytes).into_owned();
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
            content: vec![code_agent_core::ToolContent::Text(text)],
            is_error,
            metadata,
        })
    }
}

async fn read_capped<R: tokio::io::AsyncRead + Unpin>(r: &mut R, cap: usize) -> Vec<u8> {
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
                // Continue draining so the child does not block on a full pipe,
                // but discard data once we've reached the cap.
            }
            Err(_) => break,
        }
    }
    buf
}

fn redact(cmd: &str) -> String {
    let mut out = cmd.to_string();
    for needle in SECRET_ENV_PATTERNS {
        let n = needle.to_ascii_lowercase();
        if out.to_ascii_lowercase().contains(&n) {
            out = format!("<redacted: contains pattern {n}>");
            break;
        }
    }
    out
}
