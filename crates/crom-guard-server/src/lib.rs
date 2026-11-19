//! `crom-guard-server`: the local Guard service (#357). Serves the MCP
//! Streamable HTTP agent endpoint and the admin API on separate listeners
//! with separate credentials (ADR 0002 §2.3). See `docs/guard/server.md`.
//!
//! Must depend on no `crom-*` crate other than `crom-guard`
//! (`scripts/check-guard-deps.sh`).

#![forbid(unsafe_code)]

mod admin;
mod admin_token;
mod agent;
pub mod cli;
pub mod config;
mod limiter;
mod net;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use crom_guard::{
    AppId, AuditError, CedarPdp, Clock, CredentialBroker, CredentialLoadError, EnvCredential,
    EnvCredentialBroker, FileAuditSink, FileCredentialBroker, GrantStoreConfig, Guard,
    MemoryGrantStore, OperationRegistry, PdpLoadError, SystemClock,
};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub use admin_token::{AdminToken, AdminTokenError};
pub use agent::{RATE_LIMITED, SESSION_ERROR};
pub use config::{AdminListen, Config, ConfigError};

/// Path of the MCP endpoint on the agent listener.
pub const MCP_PATH: &str = "/mcp";

/// Either broker, kept concretely so the admin API can reload it.
enum Broker {
    File(Arc<FileCredentialBroker>),
    Env(Arc<EnvCredentialBroker>),
}

impl Broker {
    fn reload(&self) -> Result<(), CredentialLoadError> {
        match self {
            Self::File(b) => b.reload(),
            Self::Env(b) => b.reload(),
        }
    }

    fn as_dyn(&self) -> Arc<dyn CredentialBroker> {
        match self {
            Self::File(b) => b.clone(),
            Self::Env(b) => b.clone(),
        }
    }
}

/// State shared by both listeners.
pub(crate) struct Shared {
    guard: Arc<Guard>,
    store: Arc<MemoryGrantStore>,
    audit: Arc<FileAuditSink>,
    broker: Broker,
    admin_token: AdminToken,
    app_id: AppId,
    clock: Arc<dyn Clock>,
    /// Every in-flight `Guard::call`; shutdown waits for all of them.
    calls: TaskTracker,
    limiter: limiter::DenyLimiter,
}

/// Why the server refused to start. Messages never carry secrets.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("policy: {0}")]
    Policy(#[from] PdpLoadError),
    #[error("credentials: {0}")]
    Credentials(#[from] CredentialLoadError),
    #[error("admin token: {0}")]
    AdminToken(#[from] AdminTokenError),
    #[error("audit log: {0}")]
    Audit(#[from] AuditError),
    #[error("{what}: {source}")]
    Bind {
        what: &'static str,
        source: std::io::Error,
    },
}

/// `Bearer <token>` (scheme case-insensitive) → token.
pub(crate) fn strip_bearer(value: &str) -> Option<&str> {
    let (scheme, rest) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| rest.trim_start())
}

/// Where the admin listener ended up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminAddr {
    Tcp(SocketAddr),
    Unix(std::path::PathBuf),
}

/// A running server. Dropping it does not stop the server; call
/// [`shutdown`](Self::shutdown).
#[derive(Debug)]
pub struct ServerHandle {
    pub agent_addr: SocketAddr,
    pub admin_addr: AdminAddr,
    stop: CancellationToken,
    shared: SharedDebug,
    done: tokio::task::JoinHandle<()>,
}

/// Opaque wrapper so `ServerHandle` can be `Debug` without exposing state.
struct SharedDebug(Arc<Shared>);

impl std::fmt::Debug for SharedDebug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Shared")
    }
}

impl ServerHandle {
    /// Stops accepting connections, lets every in-flight request and Guard
    /// call finish, then returns. Never cancels a call.
    pub async fn shutdown(self) {
        self.stop.cancel();
        let _ = self.done.await;
    }

    /// Calls currently executing in Guard.
    pub fn in_flight(&self) -> usize {
        self.shared.0.calls.len()
    }

    /// True once Guard has latched closed (see `docs/guard/pipeline.md`).
    pub fn is_failed_closed(&self) -> bool {
        self.shared.0.guard.is_failed_closed()
    }
}

/// Loads everything `config` names, refusing to start on any error
/// (invalid policy, unsafe credential or token file, unverifiable audit
/// log, admin socket not 0600), then binds both listeners.
pub async fn start(
    config: Config,
    registry: OperationRegistry,
) -> Result<ServerHandle, StartError> {
    config.validate()?;
    let pdp = CedarPdp::from_files(
        &config.policy.namespace,
        &config.policy.schema,
        &config.policy.policies,
    )?;
    let broker = match &config.credentials.file {
        Some(path) => Broker::File(Arc::new(FileCredentialBroker::open(path)?)),
        None => Broker::Env(Arc::new(EnvCredentialBroker::from_env(
            config
                .credentials
                .env
                .iter()
                .map(|c| EnvCredential {
                    id: c.id.clone(),
                    var: c.var.clone(),
                    actions: c.actions.clone(),
                })
                .collect(),
        )?)),
    };
    let admin_token = match (&config.admin.token_file, &config.admin.token_env) {
        (Some(path), _) => AdminToken::from_file(path)?,
        (None, Some(var)) => AdminToken::from_env(var)?,
        (None, None) => unreachable!("validated"),
    };
    let audit = Arc::new(FileAuditSink::open(&config.audit.path)?);

    if registry.is_empty() {
        tracing::warn!("no protected operations are registered; every tools/call will be denied");
    }
    let mut store_config = GrantStoreConfig::new(registry.action_ids());
    store_config.max_ttl_seconds = config.grants.max_ttl_seconds;
    let store = Arc::new(MemoryGrantStore::new(store_config));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let resolve_timeout = Duration::from_millis(config.limits.resolve_timeout_ms);
    let guard = Guard::new(
        registry,
        store.clone(),
        Arc::new(pdp),
        broker.as_dyn(),
        audit.clone(),
        clock.clone(),
    )
    .with_resolve_timer(move || Box::pin(tokio::time::sleep(resolve_timeout)));

    let shared = Arc::new(Shared {
        guard: Arc::new(guard),
        store,
        audit,
        broker,
        admin_token,
        app_id: config.admin.app_id.clone(),
        clock,
        calls: TaskTracker::new(),
        limiter: limiter::DenyLimiter::new(
            config.limits.deny_burst,
            config.limits.deny_refill_per_second,
        ),
    });

    let bind = |what| move |source| StartError::Bind { what, source };
    let agent_listener = tokio::net::TcpListener::bind(config.agent.listen)
        .await
        .map_err(bind("agent listener"))?;
    let agent_addr = agent_listener
        .local_addr()
        .map_err(bind("agent listener"))?;
    let (admin_listener, admin_addr) = match &config.admin.listen {
        AdminListen::Tcp(a) => {
            let l = tokio::net::TcpListener::bind(a)
                .await
                .map_err(bind("admin listener"))?;
            let addr = l.local_addr().map_err(bind("admin listener"))?;
            (net::Listener::Tcp(l), AdminAddr::Tcp(addr))
        }
        AdminListen::Unix(p) => (
            net::Listener::bind_unix(p).map_err(bind("admin socket"))?,
            AdminAddr::Unix(p.clone()),
        ),
    };

    let limits = &config.limits;
    let read_timeout = Duration::from_millis(limits.request_read_timeout_ms);
    let agent = agent_router(&shared, &config, read_timeout);
    let admin = admin::router(shared.clone()).layer(axum::middleware::from_fn({
        let limit = limits.admin_max_request_bytes;
        move |req, next| net::read_body(limit, read_timeout, req, next)
    }));

    let (agent_conns, admin_conns) = (limits.max_connections, limits.admin_max_connections);
    let stop = CancellationToken::new();
    let done = tokio::spawn({
        let stop = stop.clone();
        let shared = shared.clone();
        async move {
            tokio::join!(
                net::serve(
                    net::Listener::Tcp(agent_listener),
                    agent,
                    read_timeout,
                    agent_conns,
                    stop.clone()
                ),
                net::serve(
                    admin_listener,
                    admin,
                    read_timeout,
                    admin_conns,
                    stop.clone()
                ),
            );
            // Connections are closed; calls spawned from them still finish.
            shared.calls.close();
            shared.calls.wait().await;
            tracing::info!("crom-guard stopped");
        }
    });
    tracing::info!(%agent_addr, admin = ?admin_addr, "crom-guard listening");
    Ok(ServerHandle {
        agent_addr,
        admin_addr,
        stop,
        shared: SharedDebug(shared),
        done,
    })
}

fn agent_router(shared: &Arc<Shared>, config: &Config, read_timeout: Duration) -> Router {
    let mut mcp = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_max_request_body_bytes(config.limits.max_request_bytes)
        // No CORS: any request carrying an `Origin` header is refused.
        .enforce_origin_validation();
    if !config.agent.allowed_hosts.is_empty() {
        mcp = mcp.with_allowed_hosts(config.agent.allowed_hosts.clone());
    }
    let handler = agent::AgentHandler {
        shared: shared.clone(),
    };
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(NeverSessionManager::default()),
        mcp,
    );
    let limit = config.limits.max_request_bytes;
    Router::new()
        .route_service(MCP_PATH, service)
        .layer(axum::middleware::from_fn(move |req, next| {
            net::read_body(limit, read_timeout, req, next)
        }))
}
