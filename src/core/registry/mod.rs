//! Owns the supervisor's authoritative task membership.
//!
//! Runtime management, static runs, and controller admission send commands here through [`SupervisorCore`](super::runtime::SupervisorCore).
//! The listener gives each command a registry decision.
//! It also receives actor completion signals.
//!
//! ```text
//! SupervisorCore ─────────────► command queue ─────► listener
//! listener ───────────────────► admission ─────────► state ───────────► actor runtime
//! actor completion identity ──► completion queue ──► listener
//! listener ───────────────────► removal ───────────► terminal state ──► outcome and cleanup
//! ```
//!
//! `state` maps each [`TaskId`](crate::TaskId) and name to one lifecycle entry.
//! An entry moves from registered to removing before it disappears.
//! The winning removal claim owns the actor handle.
//! Joins may run outside the listener, but terminal removal always returns to the shared state.
//!
//! Management commands, actor completion, and control use separate channels.
//! A full management queue does not discard actor completion or control input.
//! A shutdown fence replies after the listener processes commands committed before admission closed.
//! Lifecycle events are observations.
//! Direct replies, completion latches, and watched outcomes carry registry results.
//! The completion, control, and reaper channels are intentionally unbounded.
//! Each accepted actor emits at most one completion identity.
//! Each force-abort transfer emits one reaper future.
//! The shared shutdown path emits at most one fence and one reaper close.
//! Configured limits bound live ownership rather than channel capacity.
//!
//! A force-aborted attempt can outlive membership.
//! `scheduler` keeps its name, activity, and user values until the physical attempt exits.

use std::{num::NonZeroUsize, sync::Arc, time::Duration};

use tokio::sync::{Notify, RwLock, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{core::TaskDefaults, events::Bus};

mod admission;
mod completion;
mod listener;
mod protocol;
mod query;
mod removal;
mod scheduler;
mod state;

pub(crate) use completion::{OutcomeTx, RemovalCompletion};
/// Wire types shared with runtime management.
pub(crate) use protocol::{
    AddBatchItem, AddReplyRx, CancelDecision, CancelReplyRx, RegistryCommand, RemoveReplyRx,
};

use listener::ListenerState;
use removal::PendingJoins;
use scheduler::ActorRuntime;
use state::Inner;

#[cfg(test)]
use removal::{JoinCompletion, RemovalReport, TerminalFinalizer};
#[cfg(test)]
use state::{Entry, EntryState, Handle, HandleCleanup};

/// Supervisor-owned registry service.
pub(crate) struct Registry {
    /// Shared membership indexes and entry state.
    state: Arc<RwLock<Inner>>,
    /// Best-effort task lifecycle event bus.
    bus: Bus,
    /// Supervisor-wide registry cancellation token.
    runtime_token: CancellationToken,
    /// Optional concurrent-attempt limit.
    semaphore: Option<Arc<Semaphore>>,
    /// Graceful actor-join deadline during removal.
    grace: Duration,
    /// Defaults for accepted task specifications.
    task_defaults: TaskDefaults,
    /// Optional limit for registered and physically reaping tasks.
    max_registered_tasks: Option<NonZeroUsize>,
    /// Notification for empty registry membership.
    empty_notify: Arc<Notify>,
    /// Removal claims with pending terminal commits.
    pending_joins: Arc<PendingJoins>,
    /// Accepted-actor scheduler and physical reaper.
    actors: ActorRuntime,
    /// Registry channel endpoints and listener task.
    listener: ListenerState,
}

impl Registry {
    /// Dormant registry whose listener and reaper have not started.
    ///
    /// [`spawn_listener`](Self::spawn_listener) starts its listener and reaper.
    pub(super) fn new(
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
}

#[cfg(test)]
mod tests;
