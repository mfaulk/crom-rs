use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;

use crate::config::{redact_text_with_secrets, McpLimits, StdioMcpServerConfig};
use crate::errors::{McpError, McpResult};
use crate::protocol::{
    parse_json_rpc_message, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
};
use crate::transport::{McpLogStream, McpServerLogEntry, McpTransport, McpTransportKind};

pub struct StdioMcpTransport {
    child: Child,
    stdin: ChildStdin,
    stdout_reader: BufReader<ChildStdout>,
    stderr_task: JoinHandle<()>,
    logs: Arc<Mutex<VecDeque<McpServerLogEntry>>>,
    max_log_bytes: usize,
    secrets: Vec<String>,
    /// Per-message cap on stdout: a misbehaving server that emits a
    /// gigantic JSON-RPC line should fail with `ResultTooLarge` instead
    /// of streaming the whole thing into memory.
    max_message_bytes: usize,
}

impl StdioMcpTransport {
    pub async fn spawn(
        config: &StdioMcpServerConfig,
        limits: McpLimits,
        secrets: Vec<String>,
    ) -> McpResult<Self> {
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in &config.env {
            cmd.env(key, value);
        }
        if let Some(cwd) = &config.cwd {
            cmd.current_dir(cwd);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| McpError::Transport(format!("failed to spawn MCP stdio server: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Transport("child stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Transport("child stdout unavailable".into()))?;
        let stderr = child.stderr.take();
        let logs = Arc::new(Mutex::new(VecDeque::new()));
        let stderr_task = spawn_stderr_task(
            stderr,
            logs.clone(),
            limits.max_stderr_bytes,
            secrets.clone(),
        );
        // 4x the structured payload limit + 64 KiB envelope slack, mirroring
        // `enforce_non_binary_result_limit` so a JSON-RPC envelope around a
        // permitted payload still fits.
        let max_message_bytes = limits
            .max_tool_result_bytes
            .saturating_mul(4)
            .max(64 * 1024);
        Ok(Self {
            child,
            stdin,
            stdout_reader: BufReader::new(stdout),
            stderr_task,
            logs,
            max_log_bytes: limits.max_stderr_bytes,
            secrets,
            max_message_bytes,
        })
    }

    async fn write_json_line<T: serde::Serialize>(&mut self, value: &T) -> McpResult<()> {
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        self.stdin.write_all(&line).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn read_response(
        &mut self,
        expected_id: &crate::protocol::JsonRpcId,
    ) -> McpResult<JsonRpcResponse> {
        loop {
            let mut line = String::new();
            let n = read_line_bounded(&mut self.stdout_reader, &mut line, self.max_message_bytes)
                .await?;
            if n == 0 {
                let status = self
                    .child
                    .try_wait()?
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "stdout closed".into());
                return Err(McpError::ServerExited(status));
            }
            let message = parse_json_rpc_message(line.trim_end())
                .map_err(|e| McpError::Transport(format!("invalid JSON-RPC on stdout: {e}")))?;
            match message {
                JsonRpcMessage::Response(resp)
                    if resp.id.as_ref() == Some(expected_id) || resp.id.is_none() =>
                {
                    return Ok(resp)
                }
                JsonRpcMessage::Response(_) | JsonRpcMessage::Notification(_) => continue,
                JsonRpcMessage::Request(req) => {
                    push_log(
                        &self.logs,
                        self.max_log_bytes,
                        McpServerLogEntry {
                            stream: McpLogStream::Stdout,
                            message: redact_text_with_secrets(
                                &format!("ignored server request {}", req.method),
                                &self.secrets,
                            ),
                            truncated: false,
                        },
                    );
                }
            }
        }
    }
}

#[async_trait]
impl McpTransport for StdioMcpTransport {
    fn kind(&self) -> McpTransportKind {
        McpTransportKind::Stdio
    }

    async fn send_request(&mut self, request: JsonRpcRequest) -> McpResult<JsonRpcResponse> {
        let id = request.id.clone();
        self.write_json_line(&request).await?;
        self.read_response(&id).await
    }

    async fn send_notification(&mut self, notification: JsonRpcNotification) -> McpResult<()> {
        self.write_json_line(&notification).await
    }

    fn set_protocol_version(&mut self, _version: Option<String>) {}

    fn drain_logs(&mut self) -> Vec<McpServerLogEntry> {
        let mut guard = self.logs.lock().unwrap_or_else(|e| e.into_inner());
        guard.drain(..).collect()
    }

    async fn shutdown(&mut self) -> McpResult<()> {
        let _ = self.stdin.shutdown().await;
        if self.child.try_wait()?.is_none() {
            let _ = self.child.kill().await;
        }
        self.stderr_task.abort();
        Ok(())
    }
}

impl Drop for StdioMcpTransport {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        self.stderr_task.abort();
    }
}

fn spawn_stderr_task(
    stderr: Option<ChildStderr>,
    logs: Arc<Mutex<VecDeque<McpServerLogEntry>>>,
    max_bytes: usize,
    secrets: Vec<String>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Some(stderr) = stderr else {
            return;
        };
        let mut lines = BufReader::new(stderr).lines();
        let mut total = 0usize;
        while let Ok(Some(line)) = lines.next_line().await {
            let mut message = redact_text_with_secrets(&line, &secrets);
            let mut truncated = false;
            if total.saturating_add(message.len()) > max_bytes {
                let remaining = max_bytes.saturating_sub(total);
                if remaining == 0 {
                    truncated = true;
                    message.clear();
                } else {
                    let mut end = remaining.min(message.len());
                    while !message.is_char_boundary(end) {
                        end -= 1;
                    }
                    message.truncate(end);
                    truncated = true;
                }
            }
            total = total.saturating_add(message.len());
            if !message.is_empty() || truncated {
                push_log(
                    &logs,
                    max_bytes,
                    McpServerLogEntry {
                        stream: McpLogStream::Stderr,
                        message,
                        truncated,
                    },
                );
            }
            if truncated {
                break;
            }
        }
    })
}

fn push_log(
    logs: &Arc<Mutex<VecDeque<McpServerLogEntry>>>,
    max_bytes: usize,
    entry: McpServerLogEntry,
) {
    let mut guard = logs.lock().unwrap_or_else(|e| e.into_inner());
    let current: usize = guard.iter().map(|e| e.message.len()).sum();
    if current < max_bytes {
        guard.push_back(entry);
    }
}

/// Read a single line of UTF-8 from `reader`, aborting with `ResultTooLarge`
/// if the line would exceed `limit` bytes. Without this guard a misbehaving
/// MCP server can stream an arbitrarily large JSON line into memory before
/// any payload-size check runs.
async fn read_line_bounded<R>(reader: &mut R, out: &mut String, limit: usize) -> McpResult<usize>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    let mut total = 0usize;
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return Ok(total);
        }
        // Look for a newline within the buffered chunk.
        let pos = buf.iter().position(|b| *b == b'\n');
        let take = match pos {
            Some(p) => p + 1,
            None => buf.len(),
        };
        // Enforce the cap before copying so we never materialize past it.
        if total.saturating_add(take) > limit {
            return Err(McpError::ResultTooLarge {
                actual: total.saturating_add(take),
                limit,
            });
        }
        let chunk = std::str::from_utf8(&buf[..take])
            .map_err(|e| McpError::Transport(format!("stdio MCP response not utf-8: {e}")))?;
        out.push_str(chunk);
        reader.consume(take);
        total += take;
        if pos.is_some() {
            return Ok(total);
        }
    }
}
