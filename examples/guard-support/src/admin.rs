//! Guard's admin API over its unix socket (`docs/guard/server.md`). Only
//! the trusted application holds the admin token.

use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// A secret string whose `Debug` never shows it.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([REDACTED])")
    }
}

#[derive(Debug)]
pub struct Admin {
    socket: PathBuf,
    token: Secret,
}

/// A grant the trusted application issued.
#[derive(Debug, Clone)]
pub struct Issued {
    pub grant_id: String,
    pub handle: Secret,
}

impl Admin {
    pub fn new(socket: PathBuf, token: Secret) -> Self {
        Self { socket, token }
    }

    async fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value)> {
        let mut stream = UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("connecting to {}", self.socket.display()))?;
        let body = match body {
            Some(b) => serde_json::to_vec(b)?,
            None => Vec::new(),
        };
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\n\
             Connection: close\r\nContent-Length: {}\r\n",
            self.token.expose(),
            body.len()
        );
        if !body.is_empty() {
            head.push_str("Content-Type: application/json\r\n");
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(&body).await?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await?;
        parse_response(&raw)
    }

    /// Calls the admin API and requires `status`. Errors name only the
    /// route, the status and Guard's error code.
    async fn expect(
        &self,
        status: u16,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        let (got, payload) = self.call(method, path, body).await?;
        if got != status {
            let code = payload.get("error").and_then(Value::as_str).unwrap_or("-");
            bail!("admin {method} {path}: HTTP {got} ({code})");
        }
        Ok(payload)
    }

    pub async fn health(&self) -> Result<()> {
        self.expect(200, "GET", "/admin/health", None)
            .await
            .map(drop)
    }

    pub async fn issue(&self, spec: &Value) -> Result<Issued> {
        let v = self
            .expect(201, "POST", "/admin/grants", Some(spec))
            .await?;
        let handle = v["handle"]
            .as_str()
            .ok_or_else(|| anyhow!("issue: no handle"))?;
        let grant_id = v["grant"]["grant_id"]
            .as_str()
            .ok_or_else(|| anyhow!("issue: no grant id"))?;
        Ok(Issued {
            grant_id: grant_id.to_owned(),
            handle: Secret::new(handle),
        })
    }

    pub async fn revoke(&self, grant_id: &str) -> Result<()> {
        let path = format!("/admin/grants/{grant_id}/revoke");
        self.expect(200, "POST", &path, None).await.map(drop)
    }

    /// Guard's own record of what is awaiting approval.
    pub async fn pending(&self, digest: &str) -> Result<Value> {
        let path = format!("/admin/approvals/{digest}");
        self.expect(200, "GET", &path, None).await
    }

    /// Approves the pending request as `approver`; returns the approval id.
    pub async fn approve(&self, digest: &str, approver: &Value, tenant: &str) -> Result<String> {
        let body = serde_json::json!({
            "action_digest": digest,
            "approver": approver,
            "approver_tenant": tenant,
            "ttl_seconds": 300,
        });
        let v = self
            .expect(201, "POST", "/admin/approvals", Some(&body))
            .await?;
        v["approval_id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("approve: no approval id"))
    }
}

/// Parses an HTTP/1.1 response read to EOF (`Connection: close`), with a
/// `Content-Length` or chunked body. An empty body is `null`.
fn parse_response(raw: &[u8]) -> Result<(u16, Value)> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed HTTP response"))?;
    let head = std::str::from_utf8(&raw[..split])?;
    let mut body = raw[split + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| anyhow!("malformed HTTP status line"))?;
    let chunked = lines.any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    if chunked {
        body = dechunk(&body)?;
    }
    let value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).context("admin response is not JSON")?
    };
    Ok((status, value))
}

fn dechunk(mut data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let eol = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| anyhow!("malformed chunk"))?;
        let size_text = std::str::from_utf8(&data[..eol])?;
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)?;
        data = &data[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        if data.len() < size + 2 {
            bail!("truncated chunk");
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Content-Length` response yields its status and JSON body.
    #[test]
    fn parses_content_length_response() {
        let raw = b"HTTP/1.1 201 Created\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"a\":\"b\"}\n";
        let (status, v) = parse_response(raw).unwrap();
        assert_eq!(status, 201);
        assert_eq!(v["a"], "b");
    }

    /// A chunked response body is reassembled before parsing.
    #[test]
    fn parses_chunked_response() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n5\r\n:\"b\"}\r\n0\r\n\r\n";
        let (status, v) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(v["a"], "b");
    }

    /// An empty body is `null`, and a response without a header terminator is an error.
    #[test]
    fn empty_body_is_null_and_garbage_is_rejected() {
        let (status, v) = parse_response(b"HTTP/1.1 204 No Content\r\n\r\n").unwrap();
        assert_eq!((status, v), (204, Value::Null));
        assert!(parse_response(b"HTTP/1.1 200 OK\r\n").is_err());
    }

    /// `Secret` and `Admin` never print the secret in `Debug`.
    #[test]
    fn debug_redacts_secrets() {
        let admin = Admin::new("/tmp/x.sock".into(), Secret::new("adm-canary"));
        assert!(!format!("{admin:?}").contains("adm-canary"));
    }
}
