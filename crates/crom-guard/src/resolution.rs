use std::future::Future;
use std::pin::pin;
use std::task::Poll;

use crate::action::ValidatedArgs;
use crate::decision::DenyReason;
use crate::ids::TenantId;
use crate::resource::ResolvedResource;
use crate::traits::{BoxFuture, ResolveError, ResourceResolver};

impl From<ResolveError> for DenyReason {
    /// Not-found and other-tenant are already one variant, so the agent sees
    /// the same `resource_not_found` code for both (MILESTONE_18 §10.2 step 3).
    fn from(e: ResolveError) -> Self {
        match e {
            ResolveError::NotFound => Self::ResourceNotFound,
            ResolveError::Unavailable => Self::EngineUnavailable,
        }
    }
}

/// Starts a fresh timer per resolution; e.g.
/// `move || Box::pin(tokio::time::sleep(timeout))`. Guard has no async
/// runtime dependency, so the host supplies the timer.
pub type ResolveTimer = Box<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// Bounds a resolver by a timer. If the timer fires first, resolution fails
/// with [`ResolveError::Unavailable`], which the pipeline denies as
/// `engine_unavailable` (#350: resolver timeout ⇒ deny).
///
/// It bounds only work done while the resolver's future is polled and
/// yields: work done synchronously in `resolve` before the future is
/// returned, or blocking inside `poll`, cannot be cut off. Resolvers must do
/// their lookups inside the future, and move blocking work off the executor
/// (e.g. `tokio::task::spawn_blocking`). A panicking resolver or executor
/// is not caught here; the pipeline (#351) must treat a panicked task as a
/// deny.
pub struct TimeoutResolver<R> {
    inner: R,
    timer: ResolveTimer,
}

impl<R> TimeoutResolver<R> {
    pub fn new(
        inner: R,
        timer: impl Fn() -> BoxFuture<'static, ()> + Send + Sync + 'static,
    ) -> Self {
        Self {
            inner,
            timer: Box::new(timer),
        }
    }

    pub fn inner(&self) -> &R {
        &self.inner
    }
}

impl<R> std::fmt::Debug for TimeoutResolver<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimeoutResolver").finish_non_exhaustive()
    }
}

impl<R: ResourceResolver> ResourceResolver for TimeoutResolver<R> {
    fn resolve<'a>(
        &'a self,
        tenant: &'a TenantId,
        args: &'a ValidatedArgs,
    ) -> BoxFuture<'a, Result<ResolvedResource, ResolveError>> {
        let work = self.inner.resolve(tenant, args);
        let timer = (self.timer)();
        Box::pin(race(work, timer))
    }
}

/// The resolver is polled first, so a result that is ready when the timer
/// fires still wins.
async fn race<W, T>(work: W, timer: T) -> Result<ResolvedResource, ResolveError>
where
    W: Future<Output = Result<ResolvedResource, ResolveError>>,
    T: Future<Output = ()>,
{
    let mut work = pin!(work);
    let mut timer = pin!(timer);
    std::future::poll_fn(|cx| {
        if let Poll::Ready(r) = work.as_mut().poll(cx) {
            return Poll::Ready(r);
        }
        if timer.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(ResolveError::Unavailable));
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::ResourceVersion;
    use crate::value::EntityUid;

    struct Fixed(Option<Result<ResolvedResource, ResolveError>>);

    impl ResourceResolver for Fixed {
        fn resolve<'a>(
            &'a self,
            _: &'a TenantId,
            _: &'a ValidatedArgs,
        ) -> BoxFuture<'a, Result<ResolvedResource, ResolveError>> {
            match self.0.clone() {
                Some(r) => Box::pin(async move { r }),
                None => Box::pin(futures::future::pending()),
            }
        }
    }

    fn found() -> ResolvedResource {
        ResolvedResource::new(
            EntityUid::new("Support::Customer", "acme-cus-001").unwrap(),
            ResourceVersion::new("1"),
        )
    }

    fn run(r: &TimeoutResolver<Fixed>) -> Result<ResolvedResource, ResolveError> {
        let tenant = TenantId::new("acme").unwrap();
        let args = ValidatedArgs::new(serde_json::json!({}));
        futures::executor::block_on(r.resolve(&tenant, &args))
    }

    /// A resolver that never completes is cut off by the timer and reported as unavailable.
    #[test]
    fn hung_resolver_times_out_as_unavailable() {
        let r = TimeoutResolver::new(Fixed(None), || Box::pin(async {}));
        assert_eq!(run(&r), Err(ResolveError::Unavailable));
    }

    /// A ready resolver result wins even when the timer is also ready.
    #[test]
    fn ready_result_beats_timer() {
        let r = TimeoutResolver::new(Fixed(Some(Ok(found()))), || Box::pin(async {}));
        assert_eq!(run(&r), Ok(found()));
        let r = TimeoutResolver::new(Fixed(Some(Err(ResolveError::NotFound))), || {
            Box::pin(futures::future::pending())
        });
        assert_eq!(run(&r), Err(ResolveError::NotFound));
    }

    /// Not-found (including other-tenant) maps to `resource_not_found` and unavailability to `engine_unavailable`.
    #[test]
    fn resolve_errors_map_to_deny_reasons() {
        assert_eq!(
            DenyReason::from(ResolveError::NotFound).code(),
            "resource_not_found"
        );
        assert_eq!(
            DenyReason::from(ResolveError::Unavailable).code(),
            "engine_unavailable"
        );
    }
}
