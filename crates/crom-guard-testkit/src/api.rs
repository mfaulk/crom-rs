use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::data::{seed, Customer, Dataset, Invoice};
use crate::ledger::{Effect, Ledger, LedgerError};

/// Header Guard's executors set so each ledger entry can be matched to a
/// Guard `executed` audit record (`UnmediatedEffect` probe).
pub const DECISION_ID_HEADER: &str = "x-guard-decision-id";

/// A bearer credential the API accepts. Only Guard's credential is
/// configured in the reference deployment; the direct ("Guard off") baseline
/// configures one for the trusted test driver instead.
#[derive(Clone)]
pub struct ApiCredential {
    pub id: String,
    pub secret: String,
}

impl std::fmt::Debug for ApiCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiCredential")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RefundRequest {
    pub amount_cents: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RefundResponse {
    pub refund_id: String,
    pub invoice: Invoice,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorBody {
    pub error: String,
}

/// The controlled business API (MILESTONE_18 §14.1). Every path is
/// tenant-scoped; an id from another tenant is indistinguishable from a
/// missing one (404).
pub struct BusinessApi {
    credentials: Vec<ApiCredential>,
    data: Mutex<State0>,
    ledger: Ledger,
}

struct State0 {
    dataset: Dataset,
    next_refund: u64,
}

impl BusinessApi {
    pub fn new(credentials: Vec<ApiCredential>, ledger: Ledger) -> Arc<Self> {
        Arc::new(Self {
            credentials,
            data: Mutex::new(State0 {
                dataset: seed(),
                next_refund: 0,
            }),
            ledger,
        })
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn snapshot(&self) -> Dataset {
        self.data.lock().unwrap().dataset.clone()
    }

    /// Trusted, in-process lookup for resolvers (#350): the application's
    /// own data access, scoped to `tenant`. Not an agent-visible read, so it
    /// is not ledgered and not reachable over HTTP.
    pub fn lookup_customer(&self, tenant: &str, id: &str) -> Option<Customer> {
        find_customer(&self.data.lock().unwrap().dataset, tenant, id)
            .ok()
            .cloned()
    }

    /// Like [`lookup_customer`](Self::lookup_customer), for invoices.
    pub fn lookup_invoice(&self, tenant: &str, id: &str) -> Option<Invoice> {
        let data = self.data.lock().unwrap();
        invoice_index(&data.dataset, tenant, id)
            .ok()
            .map(|i| data.dataset.invoices[i].clone())
    }

    /// Changes a customer out of band, as the application itself would
    /// (e.g. between authorize and execute in TOCTOU tests), and bumps its
    /// version. Not ledgered. Returns `false` if `tenant` has no such
    /// customer.
    pub fn update_customer(&self, tenant: &str, id: &str, f: impl FnOnce(&mut Customer)) -> bool {
        let mut data = self.data.lock().unwrap();
        let Some(c) = data
            .dataset
            .customers
            .iter_mut()
            .find(|c| c.tenant == tenant && c.id == id)
        else {
            return false;
        };
        f(c);
        c.version += 1;
        true
    }

    /// Like [`update_customer`](Self::update_customer), for invoices.
    pub fn update_invoice(&self, tenant: &str, id: &str, f: impl FnOnce(&mut Invoice)) -> bool {
        let mut data = self.data.lock().unwrap();
        let Ok(i) = invoice_index(&data.dataset, tenant, id) else {
            return false;
        };
        let inv = &mut data.dataset.invoices[i];
        f(inv);
        inv.version += 1;
        true
    }

    /// Restores the seed data and empties the ledger.
    pub fn reset(&self) -> Result<(), LedgerError> {
        let mut data = self.data.lock().unwrap();
        data.dataset = seed();
        data.next_refund = 0;
        self.ledger.clear()
    }

    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/tenants/{tenant}/customers/{id}", get(customer_read))
            .route(
                "/tenants/{tenant}/customers/{id}/invoices",
                get(invoice_list),
            )
            .route("/tenants/{tenant}/invoices/{id}", get(invoice_read))
            .route(
                "/tenants/{tenant}/invoices/{id}/refunds",
                post(refund_issue),
            )
            .route("/tenants/{tenant}/customers/export", post(customer_export))
            .with_state(self.clone())
    }

    /// Serves on `addr` (use port 0 for an ephemeral loopback port) until
    /// the returned handle is dropped.
    pub async fn serve(self: &Arc<Self>, addr: SocketAddr) -> std::io::Result<RunningApi> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let local = listener.local_addr()?;
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let app = self.router();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        Ok(RunningApi {
            addr: local,
            shutdown: Some(tx),
            task,
        })
    }

    fn authenticate(&self, headers: &HeaderMap) -> Result<String, ApiError> {
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(ApiError::Unauthorized)?;
        self.credentials
            .iter()
            .find(|c| c.secret == presented)
            .map(|c| c.id.clone())
            .ok_or(ApiError::Unauthorized)
    }

    fn record(
        &self,
        tenant: &str,
        credential_id: &str,
        headers: &HeaderMap,
        effect: Effect,
    ) -> Result<(), ApiError> {
        let decision = headers
            .get(DECISION_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        self.ledger
            .append(tenant, credential_id, decision, effect)
            .map(drop)
            .map_err(|_| ApiError::LedgerUnavailable)
    }
}

pub struct RunningApi {
    pub addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl RunningApi {
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = (&mut self.task).await;
    }
}

impl Drop for RunningApi {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApiError {
    Unauthorized,
    NotFound,
    PreconditionRequired,
    VersionMismatch,
    InvalidAmount,
    LedgerUnavailable,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = match self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            Self::PreconditionRequired => (StatusCode::PRECONDITION_REQUIRED, "if_match_required"),
            Self::VersionMismatch => (StatusCode::PRECONDITION_FAILED, "version_mismatch"),
            Self::InvalidAmount => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_amount"),
            // The effect would be unrecorded, so it is not performed.
            Self::LedgerUnavailable => (StatusCode::SERVICE_UNAVAILABLE, "ledger_unavailable"),
        };
        (status, Json(ErrorBody { error: code.into() })).into_response()
    }
}

type Api = State<Arc<BusinessApi>>;

fn find_customer<'a>(d: &'a Dataset, tenant: &str, id: &str) -> Result<&'a Customer, ApiError> {
    d.customers
        .iter()
        .find(|c| c.tenant == tenant && c.id == id)
        .ok_or(ApiError::NotFound)
}

fn invoice_index(d: &Dataset, tenant: &str, id: &str) -> Result<usize, ApiError> {
    d.invoices
        .iter()
        .position(|i| i.tenant == tenant && i.id == id)
        .ok_or(ApiError::NotFound)
}

fn etag(version: u64) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{version}\"")).expect("etag is ascii")
}

fn if_match(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::IF_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().trim_matches('"').to_owned())
}

/// Reads accept an optional `If-Match`; a stale one fails with 412.
fn check_optional_if_match(headers: &HeaderMap, version: u64) -> Result<(), ApiError> {
    match if_match(headers) {
        Some(v) if v != version.to_string() => Err(ApiError::VersionMismatch),
        _ => Ok(()),
    }
}

async fn customer_read(
    State(api): Api,
    Path((tenant, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let cred = api.authenticate(&headers)?;
    let data = api.data.lock().unwrap();
    let c = find_customer(&data.dataset, &tenant, &id)?.clone();
    check_optional_if_match(&headers, c.version)?;
    api.record(
        &tenant,
        &cred,
        &headers,
        Effect::CustomerRead {
            customer_id: c.id.clone(),
        },
    )?;
    Ok(([(header::ETAG, etag(c.version))], Json(c)).into_response())
}

async fn invoice_list(
    State(api): Api,
    Path((tenant, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let cred = api.authenticate(&headers)?;
    let data = api.data.lock().unwrap();
    let c = find_customer(&data.dataset, &tenant, &id)?;
    check_optional_if_match(&headers, c.version)?;
    let invoices: Vec<Invoice> = data
        .dataset
        .invoices
        .iter()
        .filter(|i| i.tenant == tenant && i.customer_id == c.id)
        .cloned()
        .collect();
    api.record(
        &tenant,
        &cred,
        &headers,
        Effect::InvoiceList {
            customer_id: c.id.clone(),
            invoice_ids: invoices.iter().map(|i| i.id.clone()).collect(),
        },
    )?;
    Ok(([(header::ETAG, etag(c.version))], Json(invoices)).into_response())
}

async fn invoice_read(
    State(api): Api,
    Path((tenant, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let cred = api.authenticate(&headers)?;
    let data = api.data.lock().unwrap();
    let inv = data.dataset.invoices[invoice_index(&data.dataset, &tenant, &id)?].clone();
    check_optional_if_match(&headers, inv.version)?;
    api.record(
        &tenant,
        &cred,
        &headers,
        Effect::InvoiceRead {
            invoice_id: inv.id.clone(),
        },
    )?;
    Ok(([(header::ETAG, etag(inv.version))], Json(inv)).into_response())
}

async fn refund_issue(
    State(api): Api,
    Path((tenant, id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(req): Json<RefundRequest>,
) -> Result<Response, ApiError> {
    let cred = api.authenticate(&headers)?;
    let mut data = api.data.lock().unwrap();
    let idx = invoice_index(&data.dataset, &tenant, &id)?;
    let expected = if_match(&headers).ok_or(ApiError::PreconditionRequired)?;
    let current = &data.dataset.invoices[idx];
    if expected != current.version.to_string() {
        return Err(ApiError::VersionMismatch);
    }
    if req.amount_cents == 0 || req.amount_cents > current.remaining_cents() {
        return Err(ApiError::InvalidAmount);
    }
    let refund_id = format!("{tenant}-ref-{:04}", data.next_refund);
    // Record before committing: if the ledger write fails the refund does
    // not happen, so the oracle never misses a state change.
    api.record(
        &tenant,
        &cred,
        &headers,
        Effect::Refund {
            invoice_id: id.clone(),
            amount_cents: req.amount_cents,
            refund_id: refund_id.clone(),
        },
    )?;
    data.next_refund += 1;
    let inv = &mut data.dataset.invoices[idx];
    inv.refunded_cents += req.amount_cents;
    inv.version += 1;
    let invoice = inv.clone();
    Ok((
        StatusCode::CREATED,
        [(header::ETAG, etag(invoice.version))],
        Json(RefundResponse { refund_id, invoice }),
    )
        .into_response())
}

async fn customer_export(
    State(api): Api,
    Path(tenant): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Vec<Customer>>, ApiError> {
    let cred = api.authenticate(&headers)?;
    let data = api.data.lock().unwrap();
    if !data.dataset.customers.iter().any(|c| c.tenant == tenant) {
        return Err(ApiError::NotFound);
    }
    let customers: Vec<Customer> = data
        .dataset
        .customers
        .iter()
        .filter(|c| c.tenant == tenant)
        .cloned()
        .collect();
    api.record(
        &tenant,
        &cred,
        &headers,
        Effect::CustomerExport {
            customer_ids: customers.iter().map(|c| c.id.clone()).collect(),
        },
    )?;
    Ok(Json(customers))
}
