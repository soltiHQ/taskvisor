//! One bounded Tokio task per accepted actor and physical force-abort reaping.
//!
//! The process-wide ownership budget bounds accepted actor tasks. Each actor
//! polls its user future inline. When logical grace expires, the registry
//! transfers the whole actor `JoinHandle` to the reaper before requesting abort;
//! its attempt permit, activity bit, label reservation, and terminal destruction
//! bundle therefore remain owned until the blocked poll or destructor physically
//! returns.
//!
//! Public shutdown does not wait for a non-empty reaper. The coordinator keeps
//! ownership while the host Tokio runtime remains alive; destroying that runtime
//! is the external lifetime boundary.

use std::{
    collections::HashMap,
    future::Future,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

use futures_util::{FutureExt, StreamExt, stream::FuturesUnordered};
use tokio::{
    sync::{mpsc, oneshot},
    task::{JoinError, JoinHandle},
};

use crate::{
    core::{actor::ActorExitReason, deferred_drop::DropBundle, runner::dispose_panic_payload},
    identity::TaskId,
};

use super::completion::RemovalCompletion;

type ActorResult = Result<ActorExitReason, ActorJoinError>;
type ScheduledFuture = Pin<Box<dyn Future<Output = ActorExitReason> + Send + 'static>>;
type ReapFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Starts a detached physical owner when a Tokio runtime is available.
///
/// Last-public-owner fallback can run after the host runtime has already been
/// destroyed. In that case polling a Tokio `JoinHandle` is impossible. Keeping
/// the already charged future retained is the bounded fail-closed alternative:
/// it preserves user-value isolation and cannot allocate beyond the global
/// ownership budget.
fn spawn_or_retain<F>(future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => drop(runtime.spawn(future)),
        Err(_no_runtime) => std::mem::forget(future),
    }
}

enum ReaperCommand {
    Reap(ReapFuture),
    Close,
}

#[derive(Default)]
struct ReaperState {
    by_label: HashMap<Arc<str>, Vec<ReaperActivity>>,
    records: HashMap<TaskId, Vec<ReaperRecord>>,
}

struct ReaperActivity {
    id: TaskId,
    release: RemovalCompletion,
    activity: Arc<AtomicBool>,
}

struct ReaperRecord {
    label: Arc<str>,
    physical: Option<ReapedPhysical>,
    terminal: Option<DropBundle>,
    release: RemovalCompletion,
    terminal_releases: Option<TerminalReleases>,
    duplicate_releases: Option<TerminalReleases>,
    poisoned: bool,
}

type ReapedDropJob = Box<dyn FnOnce() + Send + 'static>;

/// Type-erased actor output waiting for the registry's charged terminal bundle.
struct ReapedPhysical(Option<ReapedDropJob>);

impl ReapedPhysical {
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

struct ReadyRecord {
    bundle: DropBundle,
    physical: ReapedPhysical,
    release: RemovalCompletion,
    terminal_releases: TerminalReleases,
    duplicate_releases: Option<TerminalReleases>,
    poisoned: bool,
}

struct TerminalReleases {
    state: Option<RemovalCompletion>,
    report: RemovalCompletion,
}

impl TerminalReleases {
    fn complete(self) {
        if let Some(state) = self.state {
            state.complete_physical();
        }
        self.report.complete_physical();
    }

    fn shares_latch(&self, completion: &RemovalCompletion) -> bool {
        self.state
            .as_ref()
            .is_none_or(|state| state.shares_physical_latch(completion))
            && self.report.shares_physical_latch(completion)
    }
}

/// Metadata transferred before logical actor completion can be published.
pub(crate) struct AttemptReservation {
    id: TaskId,
    label: Arc<str>,
    activity: Arc<AtomicBool>,
    cleanup_poisoned: Arc<AtomicBool>,
    physical_release: RemovalCompletion,
}

impl AttemptReservation {
    pub(crate) fn new(
        id: TaskId,
        label: Arc<str>,
        activity: Arc<AtomicBool>,
        cleanup_poisoned: Arc<AtomicBool>,
        physical_release: RemovalCompletion,
    ) -> Self {
        Self {
            id,
            label,
            activity,
            cleanup_poisoned,
            physical_release,
        }
    }
}

/// Owns actor tasks that outlive their grace-bounded logical removal.
#[derive(Clone)]
pub(crate) struct AttemptReaper {
    tx: mpsc::UnboundedSender<ReaperCommand>,
    active: Arc<AtomicUsize>,
    state: Arc<Mutex<ReaperState>>,
}

impl AttemptReaper {
    /// Aborts and reaps a raw Tokio task. Production actor handles use
    /// [`abort_actor`](Self::abort_actor), which also retains the result channel.
    #[cfg(test)]
    pub(crate) fn abort_and_reap<T>(&self, handle: JoinHandle<T>, reservation: AttemptReservation)
    where
        T: Send + 'static,
    {
        let poison = Arc::clone(&reservation.cleanup_poisoned);
        let (id, release) = self.register(reservation);
        handle.abort();
        let future = async move { AssertUnwindSafe(handle).catch_unwind().await };
        self.submit_reap(id, release, poison, future);
    }

    fn abort_actor(
        &self,
        handle: JoinHandle<Option<ActorResult>>,
        result: Option<oneshot::Receiver<ActorResult>>,
        ready: Option<ActorResult>,
        reservation: AttemptReservation,
    ) {
        let poison = Arc::clone(&reservation.cleanup_poisoned);
        let (id, release) = self.register(reservation);
        // Registration and label reservation happen before abort can make the
        // actor wrapper publish its logical completion.
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

    fn register(&self, reservation: AttemptReservation) -> (TaskId, RemovalCompletion) {
        let AttemptReservation {
            id,
            label,
            activity,
            cleanup_poisoned: _,
            physical_release,
        } = reservation;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let release = physical_release.clone();
        state
            .by_label
            .entry(Arc::clone(&label))
            .or_default()
            .push(ReaperActivity {
                id,
                release: release.clone(),
                activity,
            });
        state.records.entry(id).or_default().push(ReaperRecord {
            label,
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
            // The coordinator may already be closing. This fallback task remains
            // the physical owner while the host runtime is alive.
            spawn_or_retain(future);
        }
    }

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
            // An inconsistent internal duplicate must not run a user destructor
            // under this lock or on the reaper coordinator.
            std::mem::forget(physical);
            return None;
        };
        {
            let record = &mut state
                .records
                .get_mut(&id)
                .expect("the matching reaper record remains present")[index];
            if record.physical.is_some() {
                // Never replace a stored result: replacement would destroy the old
                // user-bearing value while the reaper mutex is held.
                record.poisoned = true;
                std::mem::forget(physical);
                return Self::take_ready_record(&mut state, id, index);
            }
            record.physical = Some(physical);
            record.poisoned |= poisoned;
        }
        Self::take_ready_record(&mut state, id, index)
    }

    /// Attaches the registry's charged terminal bundle. This method is called
    /// from a Drop finalizer and is deliberately total and non-panicking.
    pub(crate) fn attach_terminal(
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
                        // Fixed-size defensive capacity: a second inconsistent
                        // duplicate fails this ownership slot closed. Its
                        // non-authoritative waiters are released after unlocking;
                        // the canonical latch remains tied to physical exit.
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
        if let Some(activities) = state.by_label.get_mut(record.label.as_ref()) {
            activities.retain(|entry| {
                entry.id != id || !entry.release.shares_physical_latch(&record.release)
            });
            if activities.is_empty() {
                state.by_label.remove(record.label.as_ref());
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

    fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    pub(crate) fn reserves_label(&self, label: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .by_label
            .contains_key(label)
    }

    /// Snapshots reaper conflicts for one admission batch under one lock.
    pub(crate) fn reserves_labels<'a>(
        &self,
        labels: impl IntoIterator<Item = &'a str>,
    ) -> Vec<bool> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        labels
            .into_iter()
            .map(|label| state.by_label.contains_key(label))
            .collect()
    }

    pub(crate) fn is_alive(&self, label: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .by_label
            .get(label)
            .is_some_and(|activities| {
                activities
                    .iter()
                    .any(|entry| entry.activity.load(Ordering::Acquire))
            })
    }

    pub(crate) fn alive_labels(&self) -> Vec<Arc<str>> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .by_label
            .iter()
            .filter(|(_, activities)| {
                activities
                    .iter()
                    .any(|entry| entry.activity.load(Ordering::Acquire))
            })
            .map(|(label, _)| Arc::clone(label))
            .collect()
    }
}

/// Failure reported by an actor wrapper.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ActorJoinError {
    Panicked { cleanup_poisoned: bool },
    Aborted,
}

impl ActorJoinError {
    pub(super) const fn is_panic(self) -> bool {
        matches!(self, Self::Panicked { .. })
    }

    pub(super) const fn cleanup_poisoned(self) -> bool {
        matches!(
            self,
            Self::Panicked {
                cleanup_poisoned: true
            }
        )
    }

    #[cfg(test)]
    pub(super) const fn is_cancelled(self) -> bool {
        matches!(self, Self::Aborted)
    }
}

/// Registry-owned physical actor handle.
pub(super) struct ActorHandle {
    join: Option<JoinHandle<Option<ActorResult>>>,
    join_slot: Arc<Mutex<Option<JoinHandle<Option<ActorResult>>>>>,
    result: Option<oneshot::Receiver<ActorResult>>,
    ready: Option<ActorResult>,
    logical: Option<ActorResult>,
    reservation: Option<AttemptReservation>,
    reaper: AttemptReaper,
    cleanup_poisoned: Arc<AtomicBool>,
}

impl ActorHandle {
    fn load_join(&mut self) {
        if self.join.is_none() {
            self.join = self
                .join_slot
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
        }
    }

    pub(super) fn abort(&mut self) {
        self.load_join();
        let Some(join) = self.join.take() else {
            self.logical = Some(Err(ActorJoinError::Aborted));
            return;
        };
        let Some(reservation) = self.reservation.take() else {
            join.abort();
            self.logical = Some(Err(ActorJoinError::Aborted));
            return;
        };
        self.reaper
            .abort_actor(join, self.result.take(), self.ready.take(), reservation);
        self.logical = Some(Err(ActorJoinError::Aborted));
    }

    /// Genuine completion ids are sent only after this result channel is ready.
    pub(super) fn result_ready(&mut self) -> bool {
        if self.ready.is_none()
            && let Some(receiver) = self.result.as_mut()
        {
            match receiver.try_recv() {
                Ok(result) => {
                    self.ready = Some(result);
                    self.result = None;
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    self.result = None;
                }
                Err(oneshot::error::TryRecvError::Empty) => {}
            }
        }
        self.ready.is_some()
    }

    /// Resolves a physically joined wrapper without losing a result that was
    /// sent after this handle's last receiver poll but before the join became
    /// ready.
    fn complete_join(&mut self, joined: Result<Option<ActorResult>, JoinError>) -> ActorResult {
        self.join = None;
        self.reservation = None;
        let fallback = match joined {
            Ok(result) => result,
            Err(error) if error.is_panic() => {
                dispose_panic_payload(error.into_panic(), self.cleanup_poisoned.as_ref());
                Some(Err(ActorJoinError::Panicked {
                    cleanup_poisoned: self.cleanup_poisoned.load(Ordering::Acquire),
                }))
            }
            Err(_cancelled) => Some(Err(ActorJoinError::Aborted)),
        };

        // The wrapper attempts the result send before it completes. Once its
        // JoinHandle is ready, the sender is therefore either delivered or
        // closed. Re-checking here closes the race with the receiver poll at
        // the beginning of `ActorHandle::poll`.
        let _ = self.result_ready();
        self.ready
            .take()
            .or(fallback)
            .unwrap_or(Err(ActorJoinError::Aborted))
    }
}

impl Future for ActorHandle {
    type Output = ActorResult;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(result) = this.logical.take() {
            return Poll::Ready(result);
        }
        this.load_join();

        if this.ready.is_none()
            && let Some(receiver) = this.result.as_mut()
        {
            match Pin::new(receiver).poll(cx) {
                Poll::Ready(Ok(result)) => {
                    this.ready = Some(result);
                    this.result = None;
                }
                Poll::Ready(Err(_closed)) => {
                    this.result = None;
                }
                Poll::Pending => {}
            }
        }

        let Some(join) = this.join.as_mut() else {
            return Poll::Pending;
        };
        let joined = match Pin::new(join).poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(joined) => joined,
        };
        Poll::Ready(this.complete_join(joined))
    }
}

impl Drop for ActorHandle {
    fn drop(&mut self) {
        self.load_join();
        if self.join.is_some() {
            self.abort();
        }
    }
}

/// One accepted actor waiting to be spawned after registry commit.
pub(super) struct ScheduledActor {
    id: TaskId,
    future: ScheduledFuture,
    result: oneshot::Sender<ActorResult>,
    completion_tx: mpsc::UnboundedSender<TaskId>,
    join_slot: Arc<Mutex<Option<JoinHandle<Option<ActorResult>>>>>,
    cleanup_poisoned: Arc<AtomicBool>,
}

/// Registry-owned identity, latches, and physical ownership for one actor task.
pub(super) struct ActorRegistration {
    pub(super) id: TaskId,
    pub(super) label: Arc<str>,
    pub(super) activity: Arc<AtomicBool>,
    pub(super) cleanup_poisoned: Arc<AtomicBool>,
    pub(super) physical_release: RemovalCompletion,
    pub(super) reaper: AttemptReaper,
    pub(super) completion_tx: mpsc::UnboundedSender<TaskId>,
}

impl ScheduledActor {
    pub(super) fn new(
        registration: ActorRegistration,
        future: impl Future<Output = ActorExitReason> + Send + 'static,
    ) -> (Self, ActorHandle) {
        let ActorRegistration {
            id,
            label,
            activity,
            cleanup_poisoned,
            physical_release,
            reaper,
            completion_tx,
        } = registration;
        let (result_tx, result_rx) = oneshot::channel();
        let join_slot = Arc::new(Mutex::new(None));
        let handle = ActorHandle {
            join: None,
            join_slot: Arc::clone(&join_slot),
            result: Some(result_rx),
            ready: None,
            logical: None,
            reservation: Some(AttemptReservation::new(
                id,
                label,
                activity,
                Arc::clone(&cleanup_poisoned),
                physical_release,
            )),
            reaper,
            cleanup_poisoned: Arc::clone(&cleanup_poisoned),
        };
        (
            Self {
                id,
                future: Box::pin(future),
                result: result_tx,
                completion_tx,
                join_slot,
                cleanup_poisoned,
            },
            handle,
        )
    }

    fn spawn(self) {
        let Self {
            id,
            future,
            result: result_tx,
            completion_tx,
            join_slot,
            cleanup_poisoned,
        } = self;
        let join = tokio::spawn(async move {
            let result = match AssertUnwindSafe(future).catch_unwind().await {
                Ok(reason) => Ok(reason),
                Err(payload) => {
                    dispose_panic_payload(payload, cleanup_poisoned.as_ref());
                    Err(ActorJoinError::Panicked {
                        cleanup_poisoned: cleanup_poisoned.load(Ordering::Acquire),
                    })
                }
            };
            let undelivered = result_tx.send(result).err();
            let _ = completion_tx.send(id);
            undelivered
        });
        *join_slot.lock().unwrap_or_else(|error| error.into_inner()) = Some(join);
    }
}

/// Spawns accepted actors and owns the physical reaper coordinator.
pub(super) struct ActorRuntime {
    attempt_reaper: AttemptReaper,
    reaper_rx: Mutex<Option<mpsc::UnboundedReceiver<ReaperCommand>>>,
    reaper_handle: Mutex<Option<JoinHandle<()>>>,
}

impl ActorRuntime {
    pub(super) fn new() -> Self {
        let (reaper_tx, reaper_rx) = mpsc::unbounded_channel();
        Self {
            attempt_reaper: AttemptReaper {
                tx: reaper_tx,
                active: Arc::new(AtomicUsize::new(0)),
                state: Arc::new(Mutex::new(ReaperState::default())),
            },
            reaper_rx: Mutex::new(Some(reaper_rx)),
            reaper_handle: Mutex::new(None),
        }
    }

    pub(crate) fn attempt_reaper(&self) -> AttemptReaper {
        self.attempt_reaper.clone()
    }

    pub(crate) fn reaping_attempts(&self) -> usize {
        self.attempt_reaper.active()
    }

    pub(super) fn spawn(&self) {
        let mut reaper_rx = self
            .reaper_rx
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .expect("attempt reaper starts exactly once");
        let handle = tokio::spawn(async move {
            let mut active = FuturesUnordered::<ReapFuture>::new();
            let mut closing = false;
            loop {
                if closing && active.is_empty() {
                    break;
                }
                tokio::select! {
                    command = reaper_rx.recv(), if !closing => match command {
                        Some(ReaperCommand::Reap(future)) => active.push(future),
                        Some(ReaperCommand::Close) | None => {
                            closing = true;
                            reaper_rx.close();
                            while let Ok(command) = reaper_rx.try_recv() {
                                if let ReaperCommand::Reap(future) = command {
                                    active.push(future);
                                }
                            }
                        }
                    },
                    completed = active.next(), if !active.is_empty() => {
                        debug_assert!(completed.is_some());
                    }
                }
            }
        });
        *self
            .reaper_handle
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(handle);
    }

    pub(super) fn schedule(&self, actor: ScheduledActor) {
        actor.spawn();
    }

    pub(super) fn schedule_batch(&self, actors: impl IntoIterator<Item = ScheduledActor>) {
        for actor in actors {
            actor.spawn();
        }
    }

    pub(super) async fn join(&self) -> bool {
        let _ = self.attempt_reaper.tx.send(ReaperCommand::Close);
        if self.attempt_reaper.active() != 0 {
            return true;
        }
        let handle = self
            .reaper_handle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        match handle {
            Some(handle) => handle.await.is_ok(),
            None => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joined_wrapper_rechecks_result_after_an_initial_empty_probe() {
        let runtime = ActorRuntime::new();
        let cleanup_poisoned = Arc::new(AtomicBool::new(false));
        let (result_tx, result_rx) = oneshot::channel();
        let mut handle = ActorHandle {
            join: None,
            join_slot: Arc::new(Mutex::new(None)),
            result: Some(result_rx),
            ready: None,
            logical: None,
            reservation: None,
            reaper: runtime.attempt_reaper(),
            cleanup_poisoned,
        };

        assert!(
            !handle.result_ready(),
            "the regression requires the first receiver probe to be empty"
        );
        result_tx
            .send(Ok(ActorExitReason::Completed))
            .expect("the actor result receiver remains owned by the handle");

        let result = handle.complete_join(Ok(None));
        assert!(
            matches!(result, Ok(ActorExitReason::Completed)),
            "the queued actor result must win over the missing join fallback"
        );
        assert!(handle.result.is_none());
    }
}
