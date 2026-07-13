//! # Authoritative task registry
//!
//! The registry owns active task identities, names, actor handles, and final
//! cleanup. A task remains registered until its actor is joined and both
//! indexes are released.
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
//! ## Paths
//!
//! ```text
//! commands:    SupervisorCore -- bounded queue --> Registry
//!                                <-- direct reply --
//! completion: TaskActor ------ reliable signal ---> Registry
//!             Registry ------- final result ------> waiters/controller
//! events:     runtime parts --- best-effort ------> subscribers
//! ```
//!
//! ## Invariants
//!
//! - Both identity indexes change under one write lock.
//! - A name stays reserved while its entry is registered or being removed.
//! - One removal claim owns the actor join handle. Later cancellation calls can
//!   wait on the same completion signal.
//! - A static batch starts only after every name is validated and indexed.
//! - Only terminal join cleanup removes membership. Best-effort events cannot do it.
//! - Shutdown processes accepted management commands before task drain starts.

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

/// Owns registered tasks and their membership state.
///
/// It accepts management commands, receives actor completion signals, joins
/// actors, resolves watched outcomes, and publishes registry lifecycle events.
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

    /// Returns registered and removing tasks as `(id, name)` pairs, sorted by identity.
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
