//! Authoritative task indexes, entry lifecycle state, and detached-join tracking.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::completion::{OutcomeTx, RemovalCompletion};
use crate::{core::actor::ActorExitReason, identity::TaskId};

/// Registry-owned actor handle for one registered task.
pub(super) struct Handle {
    pub(super) join: JoinHandle<ActorExitReason>,
    pub(super) cancel: CancellationToken,
    pub(super) done: Option<OutcomeTx>,
    pub(super) completion: RemovalCompletion,
}

/// Lifecycle phase of one authoritative registry entry.
pub(super) enum EntryState {
    /// The actor can still be claimed by remove, completion, or shutdown.
    Registered(Handle),
    /// One owner has the actor handle and is waiting for its terminal join.
    Removing { completion: RemovalCompletion },
}

/// Authoritative membership record kept until terminal join cleanup finishes.
pub(super) struct Entry {
    pub(super) label: Arc<str>,
    pub(super) state: EntryState,
}

/// Registry indexes guarded by one lock.
///
/// Keeping both maps under the same lock keeps identity and label lookup in sync.
#[derive(Default)]
pub(super) struct Inner {
    /// Canonical task map keyed by runtime identity.
    ///
    /// Entries stay here in both `Registered` and `Removing` phases.
    pub(super) tasks: HashMap<TaskId, Entry>,

    /// Label lookup used for duplicate-name checks and label-based operations.
    pub(super) by_label: HashMap<Arc<str>, TaskId>,
}

/// Mutable state for detached join tracking.
#[derive(Default)]
struct PendingInner {
    /// Number of in-flight join reporters per task identity.
    counts: HashMap<TaskId, usize>,

    /// Labels used for shutdown diagnostics when joins do not finish in time.
    labels: HashMap<TaskId, Arc<str>>,
}

/// Tracks actor joins in flight for entries in `Removing`.
///
/// It provides shutdown diagnostics and a wait barrier.
/// The registry map remains the authority for task membership.
#[derive(Default)]
pub(super) struct PendingJoins {
    inner: Mutex<PendingInner>,
    drained: Notify,
}

impl PendingJoins {
    /// Marks one join reporter for `id` as in flight.
    pub(super) fn inc(&self, id: TaskId) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *g.counts.entry(id).or_insert(0) += 1;
    }

    /// Stores the label for an in-flight join.
    ///
    /// No-op if `id` is not currently tracked.
    pub(super) fn label(&self, id: TaskId, label: Arc<str>) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.counts.contains_key(&id) {
            g.labels.insert(id, label);
        }
    }

    /// Marks one in-flight join for `id` as finished.
    ///
    /// Wakes waiters when no joins remain.
    pub(super) fn dec(&self, id: TaskId) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = g.counts.get_mut(&id) {
            *n -= 1;
            if *n == 0 {
                g.counts.remove(&id);
                g.labels.remove(&id);
            }
        }
        if g.counts.is_empty() {
            self.drained.notify_waiters();
        }
    }

    /// Returns `true` if a join for `id` is still in flight.
    #[cfg(test)]
    pub(super) fn contains(&self, id: TaskId) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .counts
            .contains_key(&id)
    }

    /// Returns `true` if no joins are in flight.
    pub(super) fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .counts
            .is_empty()
    }

    /// Returns labels for joins still in flight.
    ///
    /// Best-effort: an id that was incremented but not labeled yet is omitted.
    pub(super) fn pending_labels(&self) -> Vec<Arc<str>> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .labels
            .values()
            .cloned()
            .collect()
    }

    /// Waits until no joins are in flight.
    ///
    /// Uses register-before-check: `notified()` is created before checking `is_empty`; a concurrent `dec` cannot lose the wakeup.
    pub(super) async fn wait_drained(&self) {
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
