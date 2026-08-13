//! Owns the supervisor's authoritative task membership.
//!
//! Runtime management, static runs, and controller admission send commands here
//! through [`SupervisorCore`](super::runtime::SupervisorCore). The listener gives
//! each command a registry decision. It also receives actor completion signals.
//!
//! ```text
//! SupervisorCore ──► command queue ──► listener
//! listener ──► admission ──► state ──► actor runtime
//! actor completion identity ──► completion queue ──► listener
//! listener ──► removal ──► terminal state ──► outcome and cleanup
//! ```
//!
//! `state` maps each [`TaskId`](crate::TaskId) and label to one lifecycle entry.
//! An entry moves from registered to removing before it disappears. The winning
//! removal claim owns the actor handle. Joins may run outside the listener, but
//! terminal removal always returns to the shared state.
//!
//! Management commands, actor completion, and control use separate channels.
//! A full management queue does not discard actor completion or control input.
//! A shutdown fence replies after the listener processes commands committed
//! before admission closed. Lifecycle events are observations. Direct replies,
//! completion latches, and watched outcomes carry registry results.
//!
//! A force-aborted attempt can outlive membership. `scheduler` keeps its label,
//! activity, and user values in the physical reaper until the attempt exits.

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
#[allow(unused_imports)]
/// Wire types shared with runtime management.
pub(crate) use protocol::{
    AddBatchItem, AddReply, AddReplyRx, CancelDecision, CancelReply, CancelReplyRx,
    RegistryCommand, RemoveReply, RemoveReplyRx,
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
    /// Publishes task lifecycle events.
    bus: Bus,
    /// Cancels registry work when the supervisor runtime stops.
    runtime_token: CancellationToken,
    /// Limits concurrent task attempts when configured.
    semaphore: Option<Arc<Semaphore>>,
    /// Bounds graceful actor joins during removal.
    grace: Duration,
    /// Supplies defaults for accepted task specifications.
    task_defaults: TaskDefaults,
    /// Limits registered and physically reaping tasks when configured.
    max_registered_tasks: Option<NonZeroUsize>,
    /// Wakes callers waiting for the membership state to become empty.
    empty_notify: Arc<Notify>,
    /// Tracks removal claims whose terminal commits are pending.
    pending_joins: Arc<PendingJoins>,
    /// Schedules accepted actors and owns physical reaping.
    actors: ActorRuntime,
    /// Owns registry channel endpoints and the listener task.
    listener: ListenerState,
}

impl Registry {
    /// Builds a dormant registry.
    ///
    /// [`spawn_listener`](Self::spawn_listener) starts its listener and reaper.
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
}

#[cfg(test)]
mod tests;
