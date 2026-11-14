use std::sync::Arc;

use async_trait::async_trait;
use crom_core::{
    with_retry_observer, ModelEvent, ModelProvider, ModelRequest, ProviderError, ProviderRetry,
    RetryObserver,
};
use futures::stream::BoxStream;
use tokio::sync::mpsc;

use crate::event::{EventVerbosity, ProviderRetryEvent, SdkEvent};
use crate::RunId;

/// Wraps the runtime's provider for one run so retries performed inside
/// `stream_turn` (which the core loop calls from a spawned task) are
/// reported on a channel the run loop drains into `SdkEvent::ProviderRetry`.
pub(crate) struct RetryObservingProvider {
    inner: Arc<dyn ModelProvider>,
    observer: RetryObserver,
}

impl RetryObservingProvider {
    pub(crate) fn wrap(
        inner: Arc<dyn ModelProvider>,
    ) -> (
        Arc<dyn ModelProvider>,
        mpsc::UnboundedReceiver<ProviderRetry>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let observer: RetryObserver = Arc::new(move |r: &ProviderRetry| {
            let _ = tx.send(r.clone());
        });
        (Arc::new(Self { inner, observer }), rx)
    }
}

#[async_trait]
impl ModelProvider for RetryObservingProvider {
    async fn stream_turn(
        &self,
        request: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
        with_retry_observer(self.observer.clone(), self.inner.stream_turn(request)).await
    }
}

/// Map a retry notification to its SDK event, honouring verbosity
/// (`Standard` and `Full` only).
pub(crate) fn retry_event(
    run_id: &RunId,
    verbosity: EventVerbosity,
    retry: ProviderRetry,
) -> Option<SdkEvent> {
    matches!(verbosity, EventVerbosity::Standard | EventVerbosity::Full).then(|| {
        SdkEvent::ProviderRetry(ProviderRetryEvent {
            run_id: run_id.clone(),
            retry,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crom_core::retry_provider_call;
    use crom_core::RetryPolicy;
    use futures::stream;
    use std::time::Duration;

    struct FlakyProvider;

    #[async_trait]
    impl ModelProvider for FlakyProvider {
        async fn stream_turn(
            &self,
            _request: ModelRequest,
        ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
            let policy = RetryPolicy {
                max_retries: 2,
                base_delay: Duration::ZERO,
                ..RetryPolicy::default()
            };
            let mut calls = 0;
            retry_provider_call("flaky", &policy, || {
                calls += 1;
                let fail = calls == 1;
                async move {
                    if fail {
                        Err(ProviderError::Connect("refused".into()))
                    } else {
                        Ok(())
                    }
                }
            })
            .await?;
            Ok(Box::pin(stream::empty()))
        }
    }

    fn request() -> ModelRequest {
        ModelRequest {
            model: "m".into(),
            system: None,
            messages: vec![],
            tools: vec![],
            deferred_tools: vec![],
            tool_search_config: None,
            max_tokens: 1,
        }
    }

    // Retries inside a wrapped provider's stream_turn arrive on the run's channel, even from a spawned task.
    #[tokio::test]
    async fn wrapped_provider_reports_retries() {
        let (provider, mut rx) = RetryObservingProvider::wrap(Arc::new(FlakyProvider));
        tokio::spawn(async move { provider.stream_turn(request()).await.map(|_| ()) })
            .await
            .unwrap()
            .unwrap();
        let retry = rx.try_recv().expect("one retry reported");
        assert_eq!(retry.provider, "flaky");
        assert_eq!(retry.attempt, 1);
        assert_eq!(retry.error_kind, "connect");
        assert!(rx.try_recv().is_err());
    }

    // Retry events are emitted at Standard and Full verbosity and suppressed at Minimal.
    #[test]
    fn retry_event_respects_verbosity() {
        let retry = ProviderRetry {
            provider: "p".into(),
            attempt: 1,
            max_retries: 3,
            delay_ms: 10,
            error_kind: "rate_limited".into(),
            message: "m".into(),
        };
        let run_id = RunId::new();
        assert!(retry_event(&run_id, EventVerbosity::Minimal, retry.clone()).is_none());
        assert!(retry_event(&run_id, EventVerbosity::Full, retry.clone()).is_some());
        match retry_event(&run_id, EventVerbosity::Standard, retry) {
            Some(SdkEvent::ProviderRetry(e)) => {
                assert_eq!(e.retry.attempt, 1);
                let json = serde_json::to_value(SdkEvent::ProviderRetry(e)).unwrap();
                assert_eq!(json["type"], "provider_retry");
                assert_eq!(json["error_kind"], "rate_limited");
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
