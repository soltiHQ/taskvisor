//! Tracks every removal claim until terminal registry cleanup finishes.
//!
//! A winning claim registers here before its actor handle leaves registry state.
//! Inline completion, detached joins, and shutdown claims all use the same counter.
//! Terminal cleanup removes the registration. Shutdown uses the count as a wait barrier
//! and retains names for grace-expiry diagnostics.
//!
//! This state is not task membership and does not decide removal ownership.
//! The maps in `state` remain authoritative for both.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use tokio::sync::Notify;

use crate::identity::TaskId;

/// Mutable counts and names for removal owners awaiting terminal cleanup.
#[derive(Default)]
struct PendingInner {
    /// Number of removal owners still active for each task.
    counts: HashMap<TaskId, usize>,
    /// Task names retained for shutdown diagnostics.
    names: HashMap<TaskId, Arc<str>>,
}

/// Barrier for claimed removal owners.
#[derive(Default)]
pub(in crate::core::registry) struct PendingJoins {
    /// Counts and names protected from concurrent removal owners.
    inner: Mutex<PendingInner>,
    /// Wakes shutdown waiters when the last removal owner finishes.
    drained: Notify,
}

impl PendingJoins {
    /// Registers one removal owner and its diagnostic name atomically.
    pub(super) fn inc_with_name(&self, id: TaskId, name: Arc<str>) {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        *state.counts.entry(id).or_insert(0) += 1;
        state.names.insert(id, name);
    }

    /// Registers one removal owner without a name for a focused test.
    #[cfg(test)]
    pub(in crate::core::registry) fn inc(&self, id: TaskId) {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        *state.counts.entry(id).or_insert(0) += 1;
    }

    /// Attaches a diagnostic name to an existing test owner.
    #[cfg(test)]
    pub(in crate::core::registry) fn name(&self, id: TaskId, name: Arc<str>) {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if state.counts.contains_key(&id) {
            state.names.insert(id, name);
        }
    }

    /// Finishes one removal owner and wakes waiters when none remain.
    pub(in crate::core::registry) fn dec(&self, id: TaskId) {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(count) = state.counts.get_mut(&id) {
            if *count <= 1 {
                state.counts.remove(&id);
                state.names.remove(&id);
            } else {
                *count -= 1;
            }
        }
        if state.counts.is_empty() {
            self.drained.notify_waiters();
        }
    }

    /// Returns whether a removal owner still holds `id`.
    #[cfg(test)]
    pub(in crate::core::registry) fn contains(&self, id: TaskId) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .counts
            .contains_key(&id)
    }

    /// Returns whether every removal owner finished.
    pub(in crate::core::registry) fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .counts
            .is_empty()
    }

    /// Returns names for removal owners still active at a shutdown deadline.
    pub(super) fn pending_names(&self) -> Vec<Arc<str>> {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .names
            .values()
            .cloned()
            .collect()
    }

    /// Waits until every removal owner finishes.
    ///
    /// Notification registration happens before the empty check.
    /// This prevents a lost wakeup from the final concurrent decrement.
    pub(in crate::core::registry) async fn wait_drained(&self) {
        loop {
            let notified = self.drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_empty() {
                return;
            }
            notified.await;
        }
    }
}
