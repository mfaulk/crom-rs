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
    // Accumulate raw bytes across `fill_buf` chunks and decode UTF-8
    // exactly once after the line is complete (or EOF). The previous
    // implementation called `std::str::from_utf8` on every chunk and
    // failed with `Transport("stdio MCP response not utf-8: ...")`
    // whenever a chunk boundary fell inside a multi-byte sequence
    // (#171). A multi-byte codepoint is up to 4 bytes; with no
    // newline yet seen we have no way to know whether a trailing
    // partial codepoint belongs at the end of this chunk or the start
    // of the next, so we defer decoding until we have a definite
    // terminator.
    let mut bytes: Vec<u8> = Vec::new();
    let mut total = 0usize;
    let mut saw_newline = false;
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            break;
        }
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
        bytes.extend_from_slice(&buf[..take]);
        reader.consume(take);
        total += take;
        if pos.is_some() {
            saw_newline = true;
            break;
        }
    }
    if total == 0 && !saw_newline {
        return Ok(0);
    }
    let s = std::str::from_utf8(&bytes)
        .map_err(|e| McpError::Transport(format!("stdio MCP response not utf-8: {e}")))?;
    out.push_str(s);
    Ok(total)
}

#[cfg(test)]
mod read_line_bounded_tests {
    //! Regression coverage for #171: a UTF-8 multi-byte sequence that
    //! straddles a `fill_buf` chunk boundary must not be reported as
    //! "not utf-8".

    use super::read_line_bounded;
    use std::io::Cursor;
    use tokio::io::{AsyncBufRead, AsyncRead, ReadBuf};

    /// A `AsyncBufRead` whose `fill_buf` returns at most `chunk_size`
    /// bytes per poll. Lets us force a multi-byte codepoint to split.
    struct ChunkedReader {
        data: Vec<u8>,
        pos: usize,
        chunk_size: usize,
    }

    impl ChunkedReader {
        fn new(data: Vec<u8>, chunk_size: usize) -> Self {
            Self {
                data,
                pos: 0,
                chunk_size,
            }
        }
    }

    impl AsyncRead for ChunkedReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let remaining = self.data.len() - self.pos;
            let n = remaining.min(buf.remaining()).min(self.chunk_size);
            let s = self.pos;
            buf.put_slice(&self.data[s..s + n]);
            self.pos += n;
            std::task::Poll::Ready(Ok(()))
        }
    }

    impl AsyncBufRead for ChunkedReader {
        fn poll_fill_buf(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<&[u8]>> {
            let this = self.get_mut();
            let remaining = this.data.len() - this.pos;
            let n = remaining.min(this.chunk_size);
            let s = this.pos;
            std::task::Poll::Ready(Ok(&this.data[s..s + n]))
        }

        fn consume(self: std::pin::Pin<&mut Self>, amt: usize) {
            let this = self.get_mut();
            this.pos += amt;
        }
    }

    // The original bug: a 3-byte UTF-8 codepoint (e.g. `日`) split
    // across two `fill_buf` returns produced
    // `Err(Transport("stdio MCP response not utf-8: ..."))`.
    // Post-fix the accumulator defers UTF-8 decoding until the full
    // line is in hand, so split codepoints are reassembled correctly.
    #[tokio::test]
    async fn handles_multibyte_split_across_fill_buf_chunks() {
        // `日本` = E6 97 A5 E6 9C AC = 6 bytes followed by `\n`.
        let mut data = "日本".as_bytes().to_vec();
        data.push(b'\n');
        // Chunk size 2 forces a split inside each 3-byte codepoint.
        let mut reader = ChunkedReader::new(data, 2);
        let mut out = String::new();
        let n = read_line_bounded(&mut reader, &mut out, 1024)
            .await
            .expect("split utf-8 must succeed (#171)");
        assert_eq!(n, "日本\n".len());
        assert_eq!(out, "日本\n");
    }

    // Sanity: a single-chunk line still works.
    #[tokio::test]
    async fn reads_single_chunk_line() {
        let data = b"hello\n".to_vec();
        let mut reader = Cursor::new(data);
        let mut out = String::new();
        let n = read_line_bounded(&mut reader, &mut out, 1024)
            .await
            .unwrap();
        assert_eq!(n, 6);
        assert_eq!(out, "hello\n");
    }

    // Limit is enforced before allocation.
    #[tokio::test]
    async fn enforces_byte_cap() {
        let data = b"hello world\n".to_vec();
        let mut reader = Cursor::new(data);
        let mut out = String::new();
        let err = read_line_bounded(&mut reader, &mut out, 4)
            .await
            .expect_err("cap must be enforced");
        assert!(matches!(
            err,
            crate::errors::McpError::ResultTooLarge { .. }
        ));
    }
}
