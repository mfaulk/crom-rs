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

    /// Cancellation that fires BEFORE `cancelled()` is awaited must still
    /// resolve the wait. Regression for the #157 lost-wakeup race:
    /// `Notify::notify_waiters()` only wakes already-registered futures,
    /// so a naive `if flag { return } else { notify.notified().await }`
    /// can drop the wakeup if cancel() runs between the check and the
    /// `.await`. Drives the race by cancelling the handle FIRST and
    /// then calling cancelled() with a tight timeout — pre-fix, the
    /// `notified().await` would have hung.
    #[tokio::test]
    async fn cancelled_does_not_lose_wakeup_when_cancel_precedes_wait() {
        let h = CancellationHandle::new();
        h.cancel(); // fire BEFORE any waiter exists
        let task = tokio::spawn(async move { h.cancelled().await });
        // With the bug, this future would never resolve because no
        // waiter was registered at the time of notify_waiters(). With
        // the fix, cancelled() observes the flag and returns
        // immediately.
        tokio::time::timeout(std::time::Duration::from_millis(200), task)
            .await
            .expect("cancelled() must resolve when cancel ran first (#157)")
            .unwrap();
    }
}
