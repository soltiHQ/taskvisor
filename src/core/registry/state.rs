//! Authoritative task indexes, entry lifecycle state, and detached-join tracking.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, atomic::AtomicBool},
};

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::completion::{OutcomeTx, RemovalCompletion};
use super::scheduler::{ActorHandle, AttemptReaper};
use crate::{core::deferred_drop::DropBundle, identity::TaskId};

/// Registry-owned actor handle for one registered task.
pub(super) struct Handle {
    join: ActorHandle,
    pub(super) cancel: CancellationToken,
    pub(super) done: Option<OutcomeTx>,
    pub(super) completion: RemovalCompletion,
    /// Keeps the user task alive until its actor has reached terminal cleanup.
    ///
    /// The wrapper prevents the final library-owned `Arc` from running a user
    /// destructor on the actor task or registry listener.
    cleanup: HandleCleanup,
}

impl Handle {
    pub(super) fn new(
        join: ActorHandle,
        cancel: CancellationToken,
        done: Option<OutcomeTx>,
        completion: RemovalCompletion,
        cleanup: HandleCleanup,
    ) -> Self {
        Self {
            join,
            cancel,
            done,
            completion,
            cleanup,
        }
    }

    pub(super) fn result_ready(&mut self) -> bool {
        self.join.result_ready()
    }

    pub(super) fn join_mut(&mut self) -> &mut ActorHandle {
        &mut self.join
    }

    pub(super) fn abort(&mut self) {
        self.join.abort();
    }

    /// Separates reporting data only after the actor is physically joined or
    /// has already transferred itself to the reaper.
    ///
    /// Dropping `join` first preserves the same ownership ordering as ordinary
    /// `Handle` teardown before the charged terminal bundle is extracted.
    pub(super) fn into_report_parts(self) -> (Option<OutcomeTx>, DropBundle) {
        let Self {
            join,
            done,
            cleanup,
            ..
        } = self;
        drop(join);
        (done, cleanup.into_bundle())
    }
}

/// Couples raw registry teardown to the physical reaper.
///
/// `Handle::join` is declared before this field, so ordinary field teardown
/// first lets `ActorHandle::drop` register physical ownership. This wrapper
/// then attaches the already charged terminal bundle to that record. Normal
/// removal extracts the bundle explicitly and leaves this Drop path empty.
pub(super) struct HandleCleanup {
    id: TaskId,
    reaper: AttemptReaper,
    completion: RemovalCompletion,
    bundle: Option<DropBundle>,
}

impl HandleCleanup {
    pub(super) fn new(
        id: TaskId,
        reaper: AttemptReaper,
        completion: RemovalCompletion,
        bundle: DropBundle,
    ) -> Self {
        Self {
            id,
            reaper,
            completion,
            bundle: Some(bundle),
        }
    }

    pub(super) fn into_bundle(mut self) -> DropBundle {
        self.bundle
            .take()
            .expect("registry handle owns one terminal cleanup bundle")
    }
}

impl Drop for HandleCleanup {
    fn drop(&mut self) {
        let Some(bundle) = self.bundle.take() else {
            return;
        };
        self.reaper
            .attach_terminal(self.id, bundle, None, self.completion.clone());
        self.completion.complete_logical();
    }
}

/// Lifecycle phase of one authoritative registry entry.
pub(super) enum EntryState {
    /// The actor can still be claimed by remove, completion, or shutdown.
    Registered(Box<Handle>),
    /// One owner has the actor handle and is waiting for its terminal join.
    Removing { completion: RemovalCompletion },
}

/// Authoritative membership record kept until terminal join cleanup finishes.
pub(super) struct Entry {
    pub(super) label: Arc<str>,
    /// Authoritative indication that this task is currently inside an attempt.
    pub(super) activity: Arc<AtomicBool>,
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
            if *n <= 1 {
                g.counts.remove(&id);
                g.labels.remove(&id);
            } else {
                *n -= 1;
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
