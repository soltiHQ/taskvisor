//! # Task registry.
//!
//! Owns registered task actors and their runtime identity. The registry is the
//! authoritative owner of active membership (`TaskId -> actor` and
//! `label -> TaskId`) until terminal actor cleanup finishes.
//!
//! ## Internal layout
//!
//! - `protocol`: management commands, direct replies, and cancel decisions.
//! - `completion`: lossless watched-task and terminal-cleanup signals.
//! - `state`: the two synchronized indexes, entry state, and pending joins.
//! - `admission`: actor construction and atomic single/batch registration.
//! - `removal`: removal arbitration, actor joins, outcomes, and cleanup.
//! - `listener`: command/control/completion dispatch and listener lifecycle.
//!
//! ## Planes
//!
//! ```text
//! Management:   SupervisorCore -- bounded commands --> registry listener
//!                              <-- oneshot replies ---
//! Completion:   TaskActor ------ actor identity -----> registry listener
//!                registry ------ terminal signal ----> waiters/controller
//! Observability: runtime actors/registry -- Bus -----> subscribers
//! ```
//!
//! ## Ordering invariants
//!
//! - Both identity indexes change under one write lock.
//! - A label remains reserved while its entry is `Registered` or `Removing`.
//! - Exactly one claimant owns an actor `JoinHandle`; later cancellation callers
//!   share its terminal completion signal.
//! - Static batches validate every label and hold actor bodies behind a start
//!   gate until the complete batch is indexed and announced.
//! - `TaskRemoved` is emitted only by terminal join cleanup; lossy actor events
//!   never remove membership.
//! - A control fence drains management commands visible before admission closes.

use std::{sync::Arc, time::Duration};

use tokio::sync::{Notify, RwLock, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{core::TaskDefaults, events::Bus, identity::TaskId};

mod admission;
mod completion;
mod listener;
mod protocol;
mod removal;
mod state;

pub(crate) use completion::{OutcomeTx, RemovalCompletion};
#[allow(unused_imports)]
// Keep the pre-decomposition `core::registry` protocol surface intact.
pub(crate) use protocol::{
    AddBatchItem, AddReply, AddReplyRx, CancelDecision, CancelReply, CancelReplyRx,
    RegistryCommand, RemoveReply, RemoveReplyRx,
};

use listener::ListenerState;
use state::{Inner, PendingJoins};

#[cfg(test)]
use removal::{JoinCompletion, RemovalReport};
#[cfg(test)]
use state::{Entry, EntryState, Handle};

/// Owns registered task actors and task membership.
///
/// The registry accepts add/remove commands, receives reliable actor completion
/// signals, joins actors after removal or completion, and publishes registry-level
/// lifecycle events such as `TaskAdded` and `TaskRemoved`.
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
    empty_notify: Arc<Notify>,
    pending_joins: Arc<PendingJoins>,
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
        cmd_rx: mpsc::Receiver<RegistryCommand>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Arc::new(RwLock::new(Inner::default())),
            bus,
            runtime_token,
            semaphore,
            grace,
            task_defaults,
            empty_notify: Arc::new(Notify::new()),
            pending_joins: Arc::new(PendingJoins::default()),
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

    /// Returns registered and removing tasks as `(id, label)` pairs, sorted by identity.
    pub async fn list(&self) -> Vec<(TaskId, Arc<str>)> {
        let st = self.state.read().await;
        let mut tasks: Vec<(TaskId, Arc<str>)> = st
            .tasks
            .iter()
            .map(|(id, entry)| (*id, Arc::clone(&entry.label)))
            .collect();
        tasks.sort_by_key(|(id, _)| *id);
        tasks
    }

    /// Returns true if `id` is registered or removing.
    #[cfg(test)]
    pub async fn contains(&self, id: TaskId) -> bool {
        self.state.read().await.tasks.contains_key(&id)
    }

    /// Resolves a label to the identity currently holding it (if any).
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
