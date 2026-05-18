//! Budgets, deadlines, and cancellation.
//!
//! `RunBudgets` is a record of optional caps the runtime enforces during a
//! run. `CancellationHandle` is a small clone-friendly handle that flips a
//! shared atomic and notifies waiting tasks.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Optional run-time caps that the SDK enforces during a run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunBudgets {
    pub max_turns: Option<u32>,
    pub max_tool_calls: Option<u32>,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub max_cost_usd: Option<f64>,
    pub max_duration: Option<Duration>,
    pub max_files_changed: Option<usize>,
    pub max_patch_bytes: Option<usize>,
}

impl RunBudgets {
    pub fn unlimited() -> Self {
        Self::default()
    }

    pub fn with_max_turns(mut self, n: u32) -> Self {
        self.max_turns = Some(n);
        self
    }

    pub fn with_max_tool_calls(mut self, n: u32) -> Self {
        self.max_tool_calls = Some(n);
        self
    }

    pub fn with_max_duration(mut self, d: Duration) -> Self {
        self.max_duration = Some(d);
        self
    }

    pub fn with_max_files_changed(mut self, n: usize) -> Self {
        self.max_files_changed = Some(n);
        self
    }

    pub fn with_max_patch_bytes(mut self, n: usize) -> Self {
        self.max_patch_bytes = Some(n);
        self
    }
}

/// Cancellation handle. Cloneable; cancelling any clone fires all.
#[derive(Clone, Debug)]
pub struct CancellationHandle {
    flag: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl CancellationHandle {
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// Wait until cancellation fires. Resolves immediately if already cancelled.
    ///
    /// Registers as a waiter via `Notified::enable` *before* checking the
    /// flag, so a concurrent `cancel()` between the check and the
    /// registration cannot drop the wakeup (#157). Without this pattern
    /// `tokio::sync::Notify::notify_waiters()` is lossy — it only wakes
    /// futures already in the waiter list.
    pub async fn cancelled(&self) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        // Force registration as a waiter without polling. Any subsequent
        // notify_waiters() now reaches us.
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }

    /// Expose the inner shared atomic. Used to thread cancellation into the
    /// core `RunConfig` so the agent loop can short-circuit on cancel.
    pub fn flag(&self) -> Arc<AtomicBool> {
        self.flag.clone()
    }

    /// Expose the inner notify so consumers can wake waiting tasks.
    pub fn notify(&self) -> Arc<tokio::sync::Notify> {
        self.notify.clone()
    }
}

impl Default for CancellationHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cancelling a clone of a CancellationHandle fires the original too.
    #[tokio::test]
    async fn clone_shares_state() {
        let h = CancellationHandle::new();
        let c = h.clone();
        assert!(!h.is_cancelled());
        c.cancel();
        assert!(h.is_cancelled());
    }

    /// CancellationHandle.cancelled() resolves immediately after cancel().
    #[tokio::test]
    async fn cancelled_resolves_when_fired() {
        let h = CancellationHandle::new();
        let c = h.clone();
        let task = tokio::spawn(async move { c.cancelled().await });
        h.cancel();
        task.await.unwrap();
    }

    /// Directly demonstrate the fix's underlying primitive: a
    /// `Notified` future that has been registered via `enable()`
    /// before the corresponding `notify_waiters()` MUST observe the
    /// wakeup, even though no `.await` poll has happened in between.
    /// Without `enable()` the registration only happens on first
    /// poll, leaving a window in which `notify_waiters()` reaches no
    /// one — that is the #157 lost-wakeup shape.
    #[tokio::test]
    async fn pinned_and_enabled_notified_receives_subsequent_notify() {
        let notify = Arc::new(tokio::sync::Notify::new());
        let waiter = notify.notified();
        tokio::pin!(waiter);
        // The pattern under test: enable registers without polling.
        waiter.as_mut().enable();
        // Fire AFTER enable: should reach the registered waiter.
        notify.notify_waiters();
        tokio::time::timeout(std::time::Duration::from_millis(200), waiter)
            .await
            .expect("enabled waiter must receive notify_waiters");
    }

    /// Inverse: confirm that a fresh, never-`enable()`d / never-polled
    /// `Notified` future does NOT receive a wakeup that fired before
    /// any registration happened. Pins down the exact `tokio::Notify`
    /// semantics the #157 fix has to design around — if this ever
    /// changes upstream the lost-wakeup pattern in `cancelled()`
    /// goes away, but until then we must keep `enable()` in the body.
    #[tokio::test]
    async fn unenabled_notified_misses_prior_notify() {
        let notify = Arc::new(tokio::sync::Notify::new());
        // Fire BEFORE any waiter is created. notify_waiters() has no
        // queue to wake, so the wakeup is dropped.
        notify.notify_waiters();
        // Build the future after the fact and try to await. It should
        // NOT resolve — the notification has been lost.
        let waiter = notify.notified();
        let timed_out = tokio::time::timeout(std::time::Duration::from_millis(50), waiter)
            .await
            .is_err();
        assert!(
            timed_out,
            "Notify::notify_waiters() before waiter registration must be lost; \
             the #157 fix exists precisely because of this semantic"
        );
    }

    /// End-to-end: tight race between `cancel()` and `cancelled()`
    /// across many iterations. With the pre-fix `cancelled()`
    /// (check-flag-then-await-notified) and a multi-threaded runtime,
    /// a subset of iterations would hit the registration-after-notify
    /// window and hang; the test would time out. With the `enable()`
    /// fix in `cancelled()` the waiter is registered before the flag
    /// check, so no wakeup is dropped and every iteration completes
    /// quickly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelled_does_not_lose_wakeup_under_stress() {
        for i in 0..200 {
            let h = CancellationHandle::new();
            let h2 = h.clone();
            let waiter = tokio::spawn(async move { h.cancelled().await });
            // Give the spawned waiter a scheduling tick before cancel
            // races it. Widens the bug window enough to expose the
            // lost-wakeup against the pre-fix impl.
            tokio::task::yield_now().await;
            h2.cancel();
            tokio::time::timeout(std::time::Duration::from_millis(500), waiter)
                .await
                .unwrap_or_else(|_| panic!("cancelled() hung on iteration {i} (#157)"))
                .unwrap();
        }
    }
}
