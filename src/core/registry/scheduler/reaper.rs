//! Retains force-aborted attempts until physical ownership is safe to release.
//!
//! Logical removal can finish when a grace deadline aborts an actor, but Tokio abort is not proof that the actor has physically exited.
//! [`AttemptReaper`] registers the task name and activity before abort.
//! Admission and activity queries consult those reservations after registry membership is gone.
//!
//! Physical actor output and the terminal [`DropBundle`] can arrive in either order.
//! Reaper records join them by task identity and physical latch.
//! When both are present, the record releases its name reservation.
//! Outside the lock, the actor output is attached to the bundle with reserved cleanup capacity.
//! The bundle is sent for deferred destruction before physical waiters are released.
//!
//! [`ActorRuntime`](super::runtime::ActorRuntime) polls reaper futures in one coordinator.
//! A closed coordinator uses a detached fallback when a Tokio runtime exists.
//! Without one, ownership is retained instead of dropping user values in an uncontrolled context.

use std::{
    collections::HashMap,
    future::Future,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use futures_util::FutureExt;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::{
    core::{deferred_drop::DropBundle, registry::completion::RemovalCompletion},
    identity::TaskId,
};

use super::actor::ActorResult;

/// Type-erased reaper operation owned by the coordinator.
pub(super) type ReapFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Detached physical owner when a Tokio runtime is available.
///
/// It uses the current Tokio runtime after the coordinator closes.
/// Without a runtime, the future is retained to avoid dropping user values in place.
fn spawn_or_retain<F>(future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => drop(runtime.spawn(future)),
        Err(_no_runtime) => std::mem::forget(future),
    }
}

/// Coordinator input for physical reaping.
pub(super) enum ReaperCommand {
    /// Physical owner accepted by the coordinator.
    Reap(ReapFuture),
    /// Coordinator admission closure.
    Close,
}

/// Reaper records and name activity guarded by one lock.
#[derive(Default)]
struct ReaperState {
    /// Physical attempts grouped by reserved name.
    by_name: HashMap<Arc<str>, Vec<ReaperActivity>>,
    /// Rendezvous records grouped by task identity.
    records: HashMap<TaskId, Vec<ReaperRecord>>,
}

/// Activity metadata retained through physical exit.
struct ReaperActivity {
    /// Stable task identity.
    id: TaskId,
    /// Physical release latch for this attempt.
    release: RemovalCompletion,
    /// Current actor activity state.
    activity: Arc<AtomicBool>,
}

/// Pairing between a physical join and its terminal cleanup bundle.
struct ReaperRecord {
    /// Name reserved by this attempt.
    name: Arc<str>,
    /// Type-erased physical actor result.
    physical: Option<ReapedPhysical>,
    /// Cleanup bundle with its capacity reservation.
    terminal: Option<DropBundle>,
    /// Canonical physical release latch.
    release: RemovalCompletion,
    /// Releases attached by terminal cleanup.
    terminal_releases: Option<TerminalReleases>,
    /// One defensive set of non-canonical releases.
    duplicate_releases: Option<TerminalReleases>,
    /// Whether inconsistent or panicking cleanup was observed.
    poisoned: bool,
}

/// Deferred destructor for a type-erased actor result.
type ReapedDropJob = Box<dyn FnOnce() + Send + 'static>;

/// Type-erased actor output waiting for its terminal cleanup bundle.
struct ReapedPhysical(
    /// Destructor run only after the terminal bundle owns this value.
    Option<ReapedDropJob>,
);

impl ReapedPhysical {
    /// Erases a physical actor result while preserving its destructor.
    fn new<T: Send + 'static>(value: T) -> Self {
        Self(Some(Box::new(move || drop(value))))
    }
}

impl Drop for ReapedPhysical {
    fn drop(&mut self) {
        if let Some(job) = self.0.take() {
            job();
        }
    }
}

/// Fully matched reaper record moved outside the shared-state lock.
struct ReadyRecord {
    /// Matched bundle ready to receive the physical result.
    bundle: DropBundle,
    /// Matched physical result ready for the bundle.
    physical: ReapedPhysical,
    /// Canonical latch completed after bundle submission.
    release: RemovalCompletion,
    /// Terminal latches completed after bundle submission.
    terminal_releases: TerminalReleases,
    /// Defensive latch set completed after bundle submission.
    duplicate_releases: Option<TerminalReleases>,
    /// Poison state applied before bundle submission.
    poisoned: bool,
}

/// Physical latches completed after deferred ownership is committed.
struct TerminalReleases {
    /// Optional latch retained in registry state.
    state: Option<RemovalCompletion>,
    /// Latch returned in the removal report.
    report: RemovalCompletion,
}

impl TerminalReleases {
    /// Completes every distinct physical latch in this set.
    fn complete(self) {
        if let Some(state) = self.state {
            state.complete_physical();
        }
        self.report.complete_physical();
    }

    /// Returns whether every release aliases the canonical latch.
    fn shares_latch(&self, completion: &RemovalCompletion) -> bool {
        self.state
            .as_ref()
            .is_none_or(|state| state.shares_physical_latch(completion))
            && self.report.shares_physical_latch(completion)
    }
}

/// Metadata transferred before logical actor completion can be published.
pub(in crate::core::registry) struct AttemptReservation {
    /// Stable task identity.
    id: TaskId,
    /// Name retained through physical exit and terminal matching.
    name: Arc<str>,
    /// Current actor activity state.
    activity: Arc<AtomicBool>,
    /// Shared panic cleanup status.
    cleanup_poisoned: Arc<AtomicBool>,
    /// Latch completed after actor output is committed to deferred cleanup.
    physical_release: RemovalCompletion,
}

impl AttemptReservation {
    /// Metadata for one possible force-abort transfer.
    pub(in crate::core::registry) fn new(
        id: TaskId,
        name: Arc<str>,
        activity: Arc<AtomicBool>,
        cleanup_poisoned: Arc<AtomicBool>,
        physical_release: RemovalCompletion,
    ) -> Self {
        Self {
            id,
            name,
            activity,
            cleanup_poisoned,
            physical_release,
        }
    }
}

/// Owns actor tasks that outlive their grace-bounded logical removal.
#[derive(Clone)]
pub(in crate::core::registry) struct AttemptReaper {
    /// Command sender for the force-abort cleanup coordinator.
    tx: mpsc::UnboundedSender<ReaperCommand>,
    /// Number of physical attempts not yet committed to deferred cleanup.
    active: Arc<AtomicUsize>,
    /// Name activity and terminal matching state.
    state: Arc<Mutex<ReaperState>>,
}

impl AttemptReaper {
    /// Empty reaper for one coordinator channel.
    pub(super) fn new(tx: mpsc::UnboundedSender<ReaperCommand>) -> Self {
        Self {
            tx,
            active: Arc::new(AtomicUsize::new(0)),
            state: Arc::new(Mutex::new(ReaperState::default())),
        }
    }

    /// Aborts and reaps a raw Tokio task.
    ///
    /// Production actor handles use [`abort_actor`](Self::abort_actor).
    /// That path also retains the reliable actor result channel.
    #[cfg(test)]
    pub(in crate::core::registry) fn abort_and_reap<T>(
        &self,
        handle: JoinHandle<T>,
        reservation: AttemptReservation,
    ) where
        T: Send + 'static,
    {
        let poison = Arc::clone(&reservation.cleanup_poisoned);
        let (id, release) = self.register(reservation);
        handle.abort();
        let future = async move { AssertUnwindSafe(handle).catch_unwind().await };
        self.submit_reap(id, release, poison, future);
    }

    /// Physical ownership registration before actor abort is requested.
    ///
    /// Registration reserves the name before abort can publish logical completion.
    pub(super) fn abort_actor(
        &self,
        handle: JoinHandle<Option<ActorResult>>,
        result: Option<oneshot::Receiver<ActorResult>>,
        ready: Option<ActorResult>,
        reservation: AttemptReservation,
    ) {
        let poison = Arc::clone(&reservation.cleanup_poisoned);
        let (id, release) = self.register(reservation);
        handle.abort();
        let future = async move {
            let joined = AssertUnwindSafe(handle).catch_unwind().await;
            let received = match result {
                Some(receiver) => receiver.await.ok(),
                None => None,
            };
            (joined, received, ready)
        };
        self.submit_reap(id, release, poison, future);
    }

    /// Name reservation established before physical activity is incremented after unlock.
    fn register(&self, reservation: AttemptReservation) -> (TaskId, RemovalCompletion) {
        let AttemptReservation {
            id,
            name,
            activity,
            cleanup_poisoned: _,
            physical_release,
        } = reservation;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let release = physical_release.clone();
        state
            .by_name
            .entry(Arc::clone(&name))
            .or_default()
            .push(ReaperActivity {
                id,
                release: release.clone(),
                activity,
            });
        state.records.entry(id).or_default().push(ReaperRecord {
            name,
            physical: None,
            terminal: None,
            release: physical_release,
            terminal_releases: None,
            duplicate_releases: None,
            poisoned: false,
        });
        drop(state);
        self.active.fetch_add(1, Ordering::AcqRel);
        (id, release)
    }

    /// Physical owner sent to the coordinator or its fallback owner.
    ///
    /// A closed coordinator falls back to a detached task.
    /// If no Tokio runtime exists, the future and its owned values are retained.
    fn submit_reap<T, F>(
        &self,
        id: TaskId,
        release: RemovalCompletion,
        poison: Arc<AtomicBool>,
        future: F,
    ) where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        let reaper = self.clone();
        let future = async move {
            let physical = ReapedPhysical::new(future.await);
            let ready =
                reaper.complete_physical(id, &release, physical, poison.load(Ordering::Acquire));
            reaper.submit_ready(ready);
        }
        .boxed();
        if let Err(error) = self.tx.send(ReaperCommand::Reap(future))
            && let ReaperCommand::Reap(future) = error.0
        {
            spawn_or_retain(future);
        }
    }

    /// Physical output attachment with a complete terminal match when available.
    ///
    /// Missing and duplicate records retain unexpected user values.
    /// They never destroy those values while the reaper lock is held.
    fn complete_physical(
        &self,
        id: TaskId,
        release: &RemovalCompletion,
        physical: ReapedPhysical,
        poisoned: bool,
    ) -> Option<ReadyRecord> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let Some(index) = state.records.get(&id).and_then(|records| {
            records
                .iter()
                .position(|record| record.release.shares_physical_latch(release))
        }) else {
            std::mem::forget(physical);
            return None;
        };
        {
            let record = &mut state
                .records
                .get_mut(&id)
                .expect("the matching reaper record remains present")[index];
            if record.physical.is_some() {
                record.poisoned = true;
                std::mem::forget(physical);
                return Self::take_ready_record(&mut state, id, index);
            }
            record.physical = Some(physical);
            record.poisoned |= poisoned;
        }
        Self::take_ready_record(&mut state, id, index)
    }

    /// Registry terminal bundle attached to the matching physical owner.
    ///
    /// This drop-finalizer path handles missing and duplicate records.
    /// One non-canonical duplicate release set is retained.
    /// Later duplicates poison the record and release only their non-authoritative waiters.
    pub(in crate::core::registry) fn attach_terminal(
        &self,
        id: TaskId,
        bundle: DropBundle,
        state_release: Option<RemovalCompletion>,
        report_release: RemovalCompletion,
    ) {
        let mut immediate = Some(bundle);
        let mut immediate_releases = Some(TerminalReleases {
            state: state_release,
            report: report_release,
        });
        let mut complete_immediately = false;
        let mut complete_after_unlock = None;
        let ready = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let index = state.records.get(&id).and_then(|records| {
                let matching = immediate_releases.as_ref().and_then(|releases| {
                    records
                        .iter()
                        .position(|record| releases.shares_latch(&record.release))
                });
                matching
                    .or_else(|| records.iter().position(|record| record.terminal.is_none()))
                    .or_else(|| (!records.is_empty()).then_some(0))
            });
            match index {
                Some(index)
                    if state.records.get(&id).expect("record index exists")[index]
                        .terminal
                        .is_none() =>
                {
                    let record = &mut state.records.get_mut(&id).expect("record exists")[index];
                    record.terminal = immediate.take();
                    record.terminal_releases = immediate_releases.take();
                    Self::take_ready_record(&mut state, id, index)
                }
                Some(index) => {
                    let record = &mut state.records.get_mut(&id).expect("record exists")[index];
                    let aliases_canonical = immediate_releases
                        .as_ref()
                        .is_some_and(|releases| releases.shares_latch(&record.release));
                    if aliases_canonical {
                        immediate_releases = None;
                    } else if record.duplicate_releases.is_none() {
                        record.duplicate_releases = immediate_releases.take();
                    } else {
                        record.poisoned = true;
                        complete_after_unlock = immediate_releases.take();
                    }
                    None
                }
                None => {
                    complete_immediately = true;
                    None
                }
            }
        };
        if let Some(bundle) = immediate {
            bundle.submit();
        }
        if complete_immediately && let Some(releases) = immediate_releases {
            releases.complete();
        }
        if let Some(releases) = complete_after_unlock {
            releases.complete();
        }
        self.submit_ready(ready);
    }

    /// Fully matched record removed while the caller holds the state lock.
    fn take_ready_record(state: &mut ReaperState, id: TaskId, index: usize) -> Option<ReadyRecord> {
        let is_ready = state.records.get(&id).is_some_and(|records| {
            let Some(record) = records.get(index) else {
                return false;
            };
            record.physical.is_some()
                && record.terminal.is_some()
                && record.terminal_releases.is_some()
        });
        if !is_ready {
            return None;
        }
        let (mut record, remove_records_key) = {
            let records = state.records.get_mut(&id)?;
            let record = records.remove(index);
            (record, records.is_empty())
        };
        if remove_records_key {
            state.records.remove(&id);
        }
        if let Some(activities) = state.by_name.get_mut(record.name.as_ref()) {
            activities.retain(|entry| {
                entry.id != id || !entry.release.shares_physical_latch(&record.release)
            });
            if activities.is_empty() {
                state.by_name.remove(record.name.as_ref());
            }
        }
        Some(ReadyRecord {
            bundle: record.terminal.take()?,
            physical: record.physical.take()?,
            release: record.release,
            terminal_releases: record.terminal_releases.take()?,
            duplicate_releases: record.duplicate_releases.take(),
            poisoned: record.poisoned,
        })
    }

    /// Matched record committed to deferred cleanup before its latches complete.
    fn submit_ready(&self, ready: Option<ReadyRecord>) {
        let Some(ReadyRecord {
            mut bundle,
            physical,
            release,
            terminal_releases,
            duplicate_releases,
            poisoned,
        }) = ready
        else {
            return;
        };
        bundle.attach_physical(physical);
        if poisoned {
            bundle.poison();
        }
        bundle.submit();
        self.active.fetch_sub(1, Ordering::AcqRel);
        release.complete_physical();
        terminal_releases.complete();
        if let Some(releases) = duplicate_releases {
            releases.complete();
        }
    }

    /// Number of physical attempts not yet handed to cleanup.
    pub(super) fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    /// Whether physical reaping still reserves a name.
    pub(in crate::core::registry) fn reserves_name(&self, name: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .by_name
            .contains_key(name)
    }

    /// Snapshots name reservations for one admission batch under one lock.
    pub(in crate::core::registry) fn reserves_names<'a>(
        &self,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Vec<bool> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        names
            .into_iter()
            .map(|name| state.by_name.contains_key(name))
            .collect()
    }

    /// Whether any reaped attempt for a name is still active.
    pub(in crate::core::registry) fn is_alive(&self, name: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .by_name
            .get(name)
            .is_some_and(|activities| {
                activities
                    .iter()
                    .any(|entry| entry.activity.load(Ordering::Acquire))
            })
    }

    /// Names with at least one reaped attempt still active.
    pub(in crate::core::registry) fn alive_names(&self) -> Vec<Arc<str>> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .by_name
            .iter()
            .filter(|(_, activities)| {
                activities
                    .iter()
                    .any(|entry| entry.activity.load(Ordering::Acquire))
            })
            .map(|(name, _)| Arc::clone(name))
            .collect()
    }

    /// Admission closure for the coordinator.
    pub(super) fn close(&self) {
        let _ = self.tx.send(ReaperCommand::Close);
    }
}
