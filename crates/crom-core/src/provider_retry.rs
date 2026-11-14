//! Retry policy shared by the Anthropic, OpenAI, and Gemini providers.
//!
//! Retries are observable without threading a sink through every provider:
//! the caller wraps a provider call in [`with_retry_observer`] (a tokio
//! task-local scope) and [`retry_provider_call`] reports each retry to it.
//! Retries inside a `tokio::spawn`ed task are not observed unless the
//! spawned future is itself wrapped.

use std::future::Future;
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::errors::ProviderError;

/// Upper bound on the error text carried in a [`ProviderRetry`] event.
const RETRY_MESSAGE_LIMIT: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries after the first attempt; total attempts = 1 + `max_retries`.
    pub max_retries: u32,
    /// Delay before the first retry; doubles on each subsequent retry.
    pub base_delay: Duration,
    /// Cap on the exponential backoff delay.
    pub max_delay: Duration,
    /// Largest server `Retry-After` the policy will wait for. A longer hint
    /// surfaces the `RateLimited` error immediately instead of sleeping.
    pub max_retry_after: Duration,
    /// Randomize each backoff delay into `[d/2, d]` to avoid synchronized
    /// retries across clients. A `Retry-After` hint is used as-is instead of
    /// the backoff.
    pub jitter: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(30),
            max_retry_after: Duration::from_secs(60),
            jitter: true,
        }
    }
}

impl RetryPolicy {
    /// Policy with `max_retries` and otherwise default timing.
    pub fn with_max_retries(max_retries: u32) -> Self {
        Self {
            max_retries,
            ..Self::default()
        }
    }

    /// Un-jittered backoff before 1-based retry `retry`:
    /// `base_delay * 2^(retry - 1)`, capped at `max_delay`.
    pub fn backoff(&self, retry: u32) -> Duration {
        let shift = retry.saturating_sub(1).min(20);
        self.base_delay
            .saturating_mul(1u32 << shift)
            .min(self.max_delay)
    }

    /// Delay before 1-based retry `retry` after `err`, or `None` when the
    /// error must not be retried (not retryable, attempts exhausted, or a
    /// `Retry-After` hint longer than `max_retry_after`).
    pub fn delay_for(&self, retry: u32, err: &ProviderError) -> Option<Duration> {
        if !err.is_retryable() || retry > self.max_retries {
            return None;
        }
        match err.retry_after() {
            Some(hint) if hint > self.max_retry_after => None,
            Some(hint) => Some(hint),
            None if self.jitter => Some(jitter(self.backoff(retry))),
            None => Some(self.backoff(retry)),
        }
    }
}

/// Equal jitter: uniformly random in `[d/2, d]`. Uses std's randomly keyed
/// hasher so no RNG dependency is needed for a non-cryptographic spread.
fn jitter(d: Duration) -> Duration {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(d.as_nanos() as u64);
    let frac = (h.finish() % 1_000) as f64 / 1_000.0;
    d.mul_f64(0.5 + frac * 0.5)
}

/// One retry of a provider call, reported before the backoff sleep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRetry {
    /// Provider label, e.g. `"openai"`.
    pub provider: String,
    /// 1-based retry number (the first retry is 1).
    pub attempt: u32,
    pub max_retries: u32,
    /// Backoff applied before this retry.
    pub delay_ms: u64,
    /// [`ProviderError::kind`] of the failure that triggered the retry.
    pub error_kind: String,
    /// Truncated display of the triggering error.
    pub message: String,
}

pub type RetryObserver = Arc<dyn Fn(&ProviderRetry) + Send + Sync>;

tokio::task_local! {
    static RETRY_OBSERVER: RetryObserver;
}

/// Run `fut` with `observer` receiving every [`ProviderRetry`] emitted by
/// [`retry_provider_call`] within it.
pub async fn with_retry_observer<F: Future>(observer: RetryObserver, fut: F) -> F::Output {
    RETRY_OBSERVER.scope(observer, fut).await
}

fn notify(event: &ProviderRetry) {
    let _ = RETRY_OBSERVER.try_with(|obs| obs(event));
}

/// Call `op` until it succeeds, fails with a non-retryable error, or
/// `policy` gives up. Each retry is logged, reported to the scoped
/// observer, and preceded by the policy's backoff delay.
pub async fn retry_provider_call<T, F, Fut>(
    provider: &str,
    policy: &RetryPolicy,
    mut op: F,
) -> Result<T, ProviderError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
{
    let mut retry: u32 = 0;
    loop {
        let err = match op().await {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };
        retry += 1;
        let Some(delay) = policy.delay_for(retry, &err) else {
            return Err(err);
        };
        let mut message = err.to_string();
        if message.len() > RETRY_MESSAGE_LIMIT {
            let mut cut = RETRY_MESSAGE_LIMIT;
            while !message.is_char_boundary(cut) {
                cut -= 1;
            }
            message.truncate(cut);
        }
        let event = ProviderRetry {
            provider: provider.to_string(),
            attempt: retry,
            max_retries: policy.max_retries,
            delay_ms: delay.as_millis().min(u128::from(u64::MAX)) as u64,
            error_kind: err.kind().to_string(),
            message,
        };
        tracing::warn!(
            provider,
            attempt = event.attempt,
            max_retries = event.max_retries,
            delay_ms = event.delay_ms,
            error_kind = %event.error_kind,
            "retrying provider call"
        );
        notify(&event);
        tokio::time::sleep(delay).await;
    }
}

/// Parse a `Retry-After` header given in delta-seconds (integer or
/// decimal). HTTP-date values are not supported and yield `None`.
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let secs: f64 = value.trim().parse().ok()?;
    (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs))
}

/// Parse a millisecond `retry-after-ms` header (sent by OpenAI).
pub fn parse_retry_after_ms(value: &str) -> Option<Duration> {
    let ms: f64 = value.trim().parse().ok()?;
    (ms.is_finite() && ms >= 0.0).then(|| Duration::from_secs_f64(ms / 1_000.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    fn no_jitter(max_retries: u32) -> RetryPolicy {
        RetryPolicy {
            max_retries,
            jitter: false,
            ..RetryPolicy::default()
        }
    }

    fn server_error() -> ProviderError {
        ProviderError::from_status("t", 503, "down", None)
    }

    // from_status maps each status class to its typed variant.
    #[test]
    fn from_status_classifies_statuses() {
        use ProviderError as E;
        assert!(matches!(E::from_status("t", 401, "", None), E::Auth(_)));
        assert!(matches!(E::from_status("t", 403, "", None), E::Auth(_)));
        assert!(matches!(
            E::from_status("t", 400, "", None),
            E::InvalidRequest { status: 400, .. }
        ));
        assert!(matches!(
            E::from_status("t", 404, "", None),
            E::InvalidRequest { status: 404, .. }
        ));
        assert!(matches!(E::from_status("t", 408, "", None), E::Timeout(_)));
        let rl = E::from_status("t", 429, "", Some(Duration::from_secs(7)));
        assert!(matches!(rl, E::RateLimited { .. }));
        assert_eq!(rl.retry_after(), Some(Duration::from_secs(7)));
        assert!(matches!(
            E::from_status("t", 502, "", None),
            E::ServerError { status: 502, .. }
        ));
        assert!(matches!(E::from_status("t", 302, "", None), E::Http(_)));
    }

    // Only rate limits, timeouts, connection failures, and 5xx are retryable.
    #[test]
    fn retryable_variants() {
        assert!(ProviderError::from_status("t", 429, "", None).is_retryable());
        assert!(ProviderError::from_status("t", 408, "", None).is_retryable());
        assert!(ProviderError::from_status("t", 500, "", None).is_retryable());
        assert!(ProviderError::Connect("x".into()).is_retryable());
        assert!(!ProviderError::from_status("t", 400, "", None).is_retryable());
        assert!(!ProviderError::from_status("t", 401, "", None).is_retryable());
        assert!(!ProviderError::Http("x".into()).is_retryable());
        assert!(!ProviderError::InvalidResponse("x".into()).is_retryable());
    }

    // Backoff doubles from base_delay per retry and is capped at max_delay.
    #[test]
    fn backoff_curve_doubles_and_caps() {
        let p = no_jitter(10);
        assert_eq!(p.backoff(1), Duration::from_millis(250));
        assert_eq!(p.backoff(2), Duration::from_millis(500));
        assert_eq!(p.backoff(3), Duration::from_millis(1_000));
        assert_eq!(p.backoff(40), Duration::from_secs(30));
        assert_eq!(p.backoff(u32::MAX), Duration::from_secs(30));
    }

    // Jittered delays stay within [backoff/2, backoff].
    #[test]
    fn jitter_stays_in_bounds() {
        let p = RetryPolicy::with_max_retries(5);
        for _ in 0..200 {
            let d = p.delay_for(3, &server_error()).unwrap();
            assert!(d >= Duration::from_millis(500) && d <= Duration::from_millis(1_000));
        }
    }

    // A Retry-After hint replaces the backoff; one beyond max_retry_after stops retrying.
    #[test]
    fn retry_after_is_honoured_and_capped() {
        let p = RetryPolicy::with_max_retries(3);
        let rl = |s| ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(s)),
            message: String::new(),
        };
        assert_eq!(p.delay_for(1, &rl(5)), Some(Duration::from_secs(5)));
        assert_eq!(p.delay_for(1, &rl(0)), Some(Duration::ZERO));
        assert_eq!(p.delay_for(1, &rl(61)), None);
    }

    // Non-retryable errors and exhausted attempts yield no delay.
    #[test]
    fn delay_for_refuses_when_not_retryable_or_exhausted() {
        let p = no_jitter(2);
        assert!(p.delay_for(1, &ProviderError::Auth("x".into())).is_none());
        assert!(p.delay_for(2, &server_error()).is_some());
        assert!(p.delay_for(3, &server_error()).is_none());
    }

    // Retry-After parsing accepts delta-seconds and rejects dates and negatives.
    #[test]
    fn parses_retry_after_values() {
        assert_eq!(parse_retry_after("3"), Some(Duration::from_secs(3)));
        assert_eq!(
            parse_retry_after(" 1.5 "),
            Some(Duration::from_millis(1_500))
        );
        assert_eq!(parse_retry_after("-1"), None);
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
        assert_eq!(
            parse_retry_after_ms("250"),
            Some(Duration::from_millis(250))
        );
    }

    // The loop retries transient errors up to max_retries and reports each retry to the observer.
    #[tokio::test(start_paused = true)]
    async fn retry_loop_reports_each_retry_to_observer() {
        let seen: Arc<Mutex<Vec<ProviderRetry>>> = Arc::default();
        let sink = seen.clone();
        let observer: RetryObserver = Arc::new(move |e| sink.lock().unwrap().push(e.clone()));
        let calls = AtomicU32::new(0);
        let res: Result<(), _> = with_retry_observer(
            observer,
            retry_provider_call("test", &no_jitter(2), || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err(server_error()) }
            }),
        )
        .await;
        assert!(matches!(res, Err(ProviderError::ServerError { .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].attempt, 1);
        assert_eq!(seen[1].attempt, 2);
        assert_eq!(seen[0].provider, "test");
        assert_eq!(seen[0].error_kind, "server_error");
        assert_eq!(seen[0].delay_ms, 250);
        assert_eq!(seen[1].delay_ms, 500);
    }

    // A transient failure followed by success returns the success value.
    #[tokio::test(start_paused = true)]
    async fn retry_loop_returns_success_after_transient_failure() {
        let calls = AtomicU32::new(0);
        let res = retry_provider_call("test", &no_jitter(3), || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 {
                    Err(ProviderError::Connect("refused".into()))
                } else {
                    Ok(42)
                }
            }
        })
        .await;
        assert_eq!(res.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    // A non-retryable error is returned after a single attempt without notifying.
    #[tokio::test]
    async fn retry_loop_stops_on_non_retryable_error() {
        let calls = AtomicU32::new(0);
        let res: Result<(), _> = retry_provider_call("test", &no_jitter(3), || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(ProviderError::from_status("t", 400, "bad", None)) }
        })
        .await;
        assert!(matches!(res, Err(ProviderError::InvalidRequest { .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
