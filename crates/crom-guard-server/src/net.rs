use std::path::{Path, PathBuf};
use std::pin::pin;
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Router;
use http_body_util::{BodyExt, Limited};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub(crate) enum Listener {
    Tcp(tokio::net::TcpListener),
    #[cfg(unix)]
    Unix(tokio::net::UnixListener, PathBuf),
}

impl Listener {
    /// Binds a unix socket with mode 0600, removing a stale socket left by a
    /// previous run. Refuses any other existing file, and refuses to serve
    /// if the mode or owner cannot be verified.
    #[cfg(unix)]
    pub(crate) fn bind_unix(path: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

        let refuse = |m: String| std::io::Error::new(std::io::ErrorKind::PermissionDenied, m);
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if !meta.file_type().is_socket() {
                return Err(refuse(format!(
                    "{}: exists and is not a socket",
                    path.display()
                )));
            }
            std::fs::remove_file(path)?;
        }
        let listener = tokio::net::UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let meta = std::fs::symlink_metadata(path)?;
        let euid = rustix::process::geteuid().as_raw();
        if meta.mode() & 0o777 != 0o600 || meta.uid() != euid {
            return Err(refuse(format!(
                "{}: admin socket is not mode 0600 owned by Guard's uid",
                path.display()
            )));
        }
        Ok(Self::Unix(listener, path.to_owned()))
    }

    #[cfg(not(unix))]
    pub(crate) fn bind_unix(_path: &Path) -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "unix sockets are not supported on this platform",
        ))
    }
}

/// HTTP/1.1 only. `read_timeout` bounds reading a request's headers (here)
/// and body ([`read_body`]); it never bounds handling, so an in-flight
/// Guard call is not cut short. On `shutdown`, stops accepting, lets each
/// connection finish its current request, and returns when all are closed.
pub(crate) async fn serve(
    listener: Listener,
    router: Router,
    read_timeout: Duration,
    shutdown: CancellationToken,
) {
    let conns = TaskTracker::new();
    loop {
        let accepted = tokio::select! {
            _ = shutdown.cancelled() => break,
            a = accept(&listener) => a,
        };
        match accepted {
            Ok(io) => {
                conns.spawn(serve_conn(
                    io,
                    router.clone(),
                    read_timeout,
                    shutdown.clone(),
                ));
            }
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    #[cfg(unix)]
    if let Listener::Unix(_, path) = &listener {
        let _ = std::fs::remove_file(path);
    }
    drop(listener);
    conns.close();
    conns.wait().await;
}

trait Io: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Io for T {}

async fn accept(listener: &Listener) -> std::io::Result<Box<dyn Io>> {
    match listener {
        Listener::Tcp(l) => {
            let (s, _) = l.accept().await?;
            let _ = s.set_nodelay(true);
            Ok(Box::new(s))
        }
        #[cfg(unix)]
        Listener::Unix(l, _) => {
            let (s, _) = l.accept().await?;
            Ok(Box::new(s))
        }
    }
}

async fn serve_conn(
    io: Box<dyn Io>,
    router: Router,
    read_timeout: Duration,
    shutdown: CancellationToken,
) {
    let service = TowerToHyperService::new(router);
    let conn = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(read_timeout)
        .serve_connection(TokioIo::new(io), service);
    let mut conn = pin!(conn);
    tokio::select! {
        r = conn.as_mut() => {
            if let Err(e) = r {
                tracing::debug!(error = %e, "connection closed with error");
            }
            return;
        }
        _ = shutdown.cancelled() => conn.as_mut().graceful_shutdown(),
    }
    if let Err(e) = conn.await {
        tracing::debug!(error = %e, "connection closed with error during shutdown");
    }
}

/// Buffers the request body under a size cap and a read deadline, before
/// any handler runs: 413 when too large, 408 when too slow.
pub(crate) async fn read_body(
    limit: usize,
    timeout: Duration,
    req: Request,
    next: Next,
) -> Response {
    let (parts, body) = req.into_parts();
    let collected = tokio::time::timeout(timeout, Limited::new(body, limit).collect()).await;
    let bytes = match collected {
        Err(_) => return (StatusCode::REQUEST_TIMEOUT, "request read timed out").into_response(),
        Ok(Err(e)) if e.is::<http_body_util::LengthLimitError>() => {
            return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response()
        }
        Ok(Err(_)) => return (StatusCode::BAD_REQUEST, "request body unreadable").into_response(),
        Ok(Ok(c)) => c.to_bytes(),
    };
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}
