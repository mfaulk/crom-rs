use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::Arc;
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
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub(crate) enum Listener {
    Tcp(tokio::net::TcpListener),
    #[cfg(unix)]
    Unix(tokio::net::UnixListener, PathBuf),
}

/// The connection a request arrived on, in request extensions (and so in
/// `http::request::Parts` for MCP handlers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Peer {
    Tcp(IpAddr),
    Unix,
}

#[cfg(unix)]
fn refuse(m: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, m)
}

/// The socket's directory must not let another user replace the socket:
/// owned by Guard's uid with no group/other write, or a root-owned sticky
/// directory (e.g. `/tmp`).
#[cfg(unix)]
fn check_socket_dir(path: &Path) -> std::io::Result<()> {
    use rustix::fs::{Mode, OFlags};

    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let fd = rustix::fs::open(
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let st = rustix::fs::fstat(&fd)?;
    let euid = rustix::process::geteuid().as_raw();
    let mode = st.st_mode as u32;
    let private = st.st_uid == euid && mode & 0o022 == 0;
    let sticky_root = st.st_uid == 0 && mode & 0o1000 != 0;
    if private || sticky_root {
        Ok(())
    } else {
        Err(refuse(format!(
            "{}: admin socket directory is writable by other users \
             (need owner-only write, or a root-owned sticky directory)",
            dir.display()
        )))
    }
}

/// Binds the socket inside a fresh 0700 directory next to `path`, sets it
/// to 0600 there, and renames it into place, so it is never reachable at
/// a wider mode. (A process-wide `umask` would race other threads' file
/// creation.)
#[cfg(unix)]
fn bind_private(path: &Path) -> std::io::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().filter(|d| !d.as_os_str().is_empty());
    let staging = parent.unwrap_or(Path::new(".")).join(format!(
        ".crom-guard-bind-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::DirBuilder::new().mode(0o700).create(&staging)?;
    let staged = staging.join("s");
    let result = (|| {
        let listener = tokio::net::UnixListener::bind(&staged)?;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&staged, path)?;
        Ok(listener)
    })();
    let _ = std::fs::remove_file(&staged);
    let _ = std::fs::remove_dir(&staging);
    result
}

impl Listener {
    /// Binds a unix socket with mode 0600 (see [`bind_private`]). A
    /// stale socket from a dead run is replaced; a live one, another
    /// user's socket, or any other file at the path is refused.
    #[cfg(unix)]
    pub(crate) fn bind_unix(path: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};

        check_socket_dir(path)?;
        let euid = rustix::process::geteuid().as_raw();
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if !meta.file_type().is_socket() || meta.uid() != euid {
                return Err(refuse(format!(
                    "{}: exists and is not a socket owned by Guard's uid",
                    path.display()
                )));
            }
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                return Err(refuse(format!(
                    "{}: socket is in use by a running server",
                    path.display()
                )));
            }
            std::fs::remove_file(path)?;
        }
        let listener = bind_private(path)?;
        // lstat: never follows a symlink swapped in at the path.
        let meta = std::fs::symlink_metadata(path)?;
        if !meta.file_type().is_socket() || meta.mode() & 0o777 != 0o600 || meta.uid() != euid {
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

/// HTTP/1.1 only. At most `max_connections` connections are served at
/// once; further ones wait in the accept backlog. `read_timeout` bounds
/// reading a request's headers (here) and body ([`read_body`]); it never
/// bounds handling, so an in-flight Guard call is not cut short. On
/// `shutdown`, stops accepting, lets each connection finish its current
/// request, and returns when all are closed.
pub(crate) async fn serve(
    listener: Listener,
    router: Router,
    read_timeout: Duration,
    max_connections: usize,
    shutdown: CancellationToken,
) {
    let conns = TaskTracker::new();
    let slots = Arc::new(Semaphore::new(max_connections));
    loop {
        let permit = tokio::select! {
            _ = shutdown.cancelled() => break,
            p = slots.clone().acquire_owned() => p.expect("semaphore is never closed"),
        };
        let accepted = tokio::select! {
            _ = shutdown.cancelled() => break,
            a = accept(&listener) => a,
        };
        match accepted {
            Ok((io, peer)) => {
                let router = router.clone().layer(axum::Extension(peer));
                let shutdown = shutdown.clone();
                conns.spawn(async move {
                    serve_conn(io, router, read_timeout, shutdown).await;
                    drop(permit);
                });
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

async fn accept(listener: &Listener) -> std::io::Result<(Box<dyn Io>, Peer)> {
    match listener {
        Listener::Tcp(l) => {
            let (s, addr) = l.accept().await?;
            let _ = s.set_nodelay(true);
            Ok((Box::new(s), Peer::Tcp(addr.ip())))
        }
        #[cfg(unix)]
        Listener::Unix(l, _) => {
            let (s, _) = l.accept().await?;
            Ok((Box::new(s), Peer::Unix))
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
