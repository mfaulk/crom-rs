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
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        self.notify.notified().await;
    }

    /// Expose the inner shared atomic. Used to thread cancellation into the
    /// existing core `RunConfig` (forward-compat hook).
    #[allow(dead_code)]
    pub(crate) fn flag(&self) -> Arc<AtomicBool> {
        self.flag.clone()
    }

    /// Expose the inner notify so consumers can wake waiting tasks
    /// (forward-compat hook).
    #[allow(dead_code)]
    pub(crate) fn notify(&self) -> Arc<tokio::sync::Notify> {
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
}
