//! # Authoritative task registry
//!
//! The registry owns active task identities, names, actor handles, and final cleanup.
//! A task remains registered until bounded logical terminal cleanup releases both indexes.
//! Except for force-abort, its actor is physically joined first. A force-aborted actor instead remains reaper-owned, with its name and execution resources reserved until physical exit.
//!
//! ## Internal Layout
//!
//! - `protocol`: management commands and direct replies.
//! - `completion`: watched outcomes and terminal cleanup signals.
//! - `state`: the two synchronized indexes, entry state, and pending joins.
//! - `admission`: actor creation and atomic single or batch registration.
//! - `removal`: removal claims, actor joins, outcomes, and cleanup.
//! - `listener`: command and completion dispatch.
//!
//! ## Data Flow
//!
//! Management decisions use two separate transports:
//!
//! | Direction                             | Transport                        |
//! |---------------------------------------|----------------------------------|
//! | `SupervisorCore` -> registry listener | Bounded management command queue |
//! | Registry listener -> calling method   | Direct one-shot reply            |
//!
//! Actor joins have one owner:
//!
//! | Cleanup trigger                                | Actor-handle owner                      |
//! |------------------------------------------------|-----------------------------------------|
//! | Winning ready actor-completion claim           | Registry listener, committed inline     |
//! | Winning `Remove` or `Cancel` claim             | Detached join reporter                  |
//! | Claim made by `cancel_all_within`              | Task currently running shutdown cleanup |
//!
//! Every owner uses the same terminal commit.
//! It removes the [`TaskId`] and name from both indexes, resolves an optional watched [`TaskOutcome`](super::outcome::TaskOutcome),
//! completes cancellation waiters, and publishes the final `TaskRemoved` event.
//!
//! The registry listener serializes management admission and removal-claim decisions.
//! Shutdown can claim remaining handles directly; the shared state lock arbitrates that work with the listener.
//! Explicit cancellation joins run concurrently outside the listener. Reliable natural-completion results are already ready and commit inline; a defensive early signal falls back to a detached bounded join. Each accepted registration owns one Tokio actor task, bounded by the process-wide ownership budget and the configured registry limit. That actor polls attempts inline. A force-aborted actor transfers its whole join handle to the registry reaper so the attempt permit, activity state, and name reservation remain owned until physical termination.
//! Final index removal uses the same state lock.
//!
//! Backpressure applies only to the bounded management queue.
//! Actor completion signals and shutdown fences use separate internal unbounded channels.
//!
//! Management commands, reliable completion signals, and shutdown drive registry state.
//! Events only describe that work; losing an event cannot block cleanup or keep a controller slot occupied.
//!
//! ## Invariants
//!
//! - Both identity indexes change under one write lock.
//! - A name stays reserved while its entry is registered or being removed.
//! - One removal claim owns the actor join handle. Later cancellation calls can wait on the same completion signal.
//! - Logical completion is grace-bounded. Controller slot reuse waits for the separate physical-release latch.
//! - An accepted registration releases task bodies only after indexing and its `TaskAdded` publication and direct reply send are attempted.
//! - An accepted static batch completes those steps for every entry before releasing any task body.
//! - Only terminal logical cleanup removes membership. Best-effort events cannot do it.
//! - Before graceful task drain starts, every management command committed before admission closes reaches its direct registry decision.

use std::{
    num::NonZeroUsize,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use tokio::sync::{Notify, RwLock, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{core::TaskDefaults, events::Bus, identity::TaskId};

mod admission;
mod completion;
mod listener;
mod protocol;
mod removal;
mod scheduler;
mod state;

pub(crate) use completion::{OutcomeTx, RemovalCompletion};
#[allow(unused_imports)]
// Keep the pre-decomposition `core::registry` protocol surface intact.
pub(crate) use protocol::{
    AddBatchItem, AddReply, AddReplyRx, CancelDecision, CancelReply, CancelReplyRx,
    RegistryCommand, RemoveReply, RemoveReplyRx,
};

use listener::ListenerState;
use scheduler::ActorRuntime;
use state::{Inner, PendingJoins};

#[cfg(test)]
use removal::{JoinCompletion, RemovalReport, TerminalFinalizer};
#[cfg(test)]
use state::{Entry, EntryState, Handle, HandleCleanup};

/// Owns registered tasks and their membership state.
///
/// It accepts management commands, receives actor completion signals, joins actors, resolves watched outcomes, and publishes registry lifecycle events.
///
/// # Also
///
/// - [`TaskActor`](super::actor::TaskActor) - per-task actor spawned by the registry
/// - [`SupervisorCore`](super::runtime::SupervisorCore) - sends registry commands
/// - [`TaskOutcome`](super::outcome::TaskOutcome) - final result for watched tasks
pub(crate) struct Registry {
    state: Arc<RwLock<Inner>>,
    bus: Bus,
    runtime_token: CancellationToken,
    semaphore: Option<Arc<Semaphore>>,
    grace: Duration,
    task_defaults: TaskDefaults,
    max_registered_tasks: Option<NonZeroUsize>,
    empty_notify: Arc<Notify>,
    pending_joins: Arc<PendingJoins>,
    actors: ActorRuntime,
    listener: ListenerState,
}

impl Registry {
    /// Creates a registry with its command receiver and runtime dependencies.
    pub fn new(
        bus: Bus,
        runtime_token: CancellationToken,
        semaphore: Option<Arc<Semaphore>>,
        grace: Duration,
        task_defaults: TaskDefaults,
        max_registered_tasks: Option<NonZeroUsize>,
        cmd_rx: mpsc::Receiver<RegistryCommand>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Arc::new(RwLock::new(Inner::default())),
            bus,
            runtime_token,
            semaphore,
            grace,
            task_defaults,
            max_registered_tasks,
            empty_notify: Arc::new(Notify::new()),
            pending_joins: Arc::new(PendingJoins::default()),
            actors: ActorRuntime::new(),
            listener: ListenerState::new(cmd_rx),
        })
    }

    /// Waits until no registered or removing tasks remain.
    ///
    /// Uses register-before-check to avoid losing a wakeup.
    pub async fn wait_until_empty(&self) {
        loop {
            let notified = self.empty_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_empty().await {
                return;
            }
            notified.await;
        }
    }

    /// Returns registered and removing tasks as `(id, name)` pairs, sorted by identity.
    pub async fn list(&self) -> Vec<(TaskId, Arc<str>)> {
        let st = self.state.read().await;
        let mut tasks: Vec<(TaskId, Arc<str>)> = st
            .tasks
            .iter()
            .map(|(id, entry)| (*id, Arc::clone(&entry.label)))
            .collect();
        drop(st);
        tasks.sort_by_key(|(id, _)| *id);
        tasks
    }

    /// Returns whether the registered task currently owns a running attempt.
    pub(super) async fn is_alive(&self, name: &str) -> bool {
        let state = self.state.read().await;
        let registered = state.by_label.get(name).is_some_and(|id| {
            state
                .tasks
                .get(id)
                .is_some_and(|entry| entry.activity.load(Ordering::Acquire))
        });
        drop(state);
        registered || self.actors.attempt_reaper().is_alive(name)
    }

    /// Returns sorted names whose registered tasks currently own an attempt.
    pub(super) async fn alive_snapshot(&self) -> Vec<Arc<str>> {
        let state = self.state.read().await;
        let mut alive: Vec<_> = state
            .tasks
            .values()
            .filter(|entry| entry.activity.load(Ordering::Acquire))
            .map(|entry| Arc::clone(&entry.label))
            .collect();
        drop(state);
        alive.extend(self.actors.attempt_reaper().alive_labels());
        alive.sort_unstable();
        alive.dedup();
        alive
    }

    /// Returns true if `id` is registered or removing.
    #[cfg(test)]
    pub async fn contains(&self, id: TaskId) -> bool {
        self.state.read().await.tasks.contains_key(&id)
    }

    /// Resolves a name to its registered identity, if present.
    #[cfg(test)]
    pub async fn id_for_label(&self, name: &str) -> Option<TaskId> {
        self.state.read().await.by_label.get(name).copied()
    }

    /// Returns true if no tasks are registered or removing.
    pub async fn is_empty(&self) -> bool {
        self.state.read().await.tasks.is_empty()
    }
}

#[cfg(test)]
mod tests;
