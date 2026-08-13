//! Internal slot admission engine.
//!
//! One loop owns the ordered command receiver.
//! It applies commands, registry replies, runtime completion, and shutdown to controller state.
//!
//! ```text
//! handle ───────────► command queue ──► lifecycle driver
//! runtime results ──► lifecycle driver
//! shutdown signal ──► lifecycle driver
//!
//! lifecycle driver
//!      ├── admission ──────► runtime registry
//!      ├── identity ───────► runtime registry
//!      └── state changes ──► state and slots
//! ```
//!
//! The engine owns queued task payloads, watched outcome senders, slot state, and reverse indexes.
//! User task ownership is reserved before command intake.
//!
//! Registry replies and runtime completion are authoritative.
//! Events are best-effort and never drive slot transitions.
//!
//! The shared state lock is not held across asynchronous waits, event publication, reply delivery, or user-value destruction.

use std::sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard, OnceLock, Weak};

use tokio::sync::{Mutex, RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{
    core::{OutcomeTx, SupervisorCore, TaskOutcome, deferred_drop::DropDomain},
    events::{Bus, Event, EventKind, RejectionKind},
    identity::TaskId,
};

use super::config::ControllerConfig;

mod state;
use state::{CapacityPending, ControllerState, SlotState};

mod command;
use command::{
    AdmissionResult, CompletionResult, ControllerCommand, IdentityOperation, IdentityReply,
    RemovalResult, Submission,
};

mod handle;
pub(crate) use handle::ControllerHandle;

mod admission;
mod identity;
mod lifecycle;
use lifecycle::ControllerTask;
mod operations;
mod queue;
#[cfg(test)]
use operations::OperationSet;
use operations::TrackedOperations;

mod snapshot;

/// Owns controller state and the serialized admission loop.
pub(crate) struct Controller {
    /// Static controller configuration.
    config: ControllerConfig,
    /// Runtime control surface.
    /// `Weak` avoids extending the runtime core's lifetime during teardown.
    supervisor: Weak<SupervisorCore>,
    /// Supervisor-local ownership and destructor-isolation domain.
    drop_domain: DropDomain,
    /// Runtime event bus used for controller observability and diagnostics.
    bus: Bus,
    /// Reliable signal fired when the runtime's shared shutdown operation starts.
    shutdown_token: CancellationToken,
    /// Per-slot state, reverse indexes, and pre-commit watcher ownership.
    ///
    /// This lock is never held across `.await`, event publication, reply delivery, or user-value destruction.
    /// One lock makes aggregate limits and cross-index updates atomic.
    state: StdMutex<ControllerState>,
    /// Ordered command sender cloned into `ControllerHandle`.
    tx: mpsc::Sender<ControllerCommand>,
    /// Single-use command receiver owned by the controller loop.
    rx: RwLock<Option<mpsc::Receiver<ControllerCommand>>>,
    /// Set when the controller loop begins shutdown or exits.
    shutting_down: std::sync::atomic::AtomicBool,
    /// Single controller loop task shared by every start and join caller.
    task: OnceLock<ControllerTask>,
}

impl Controller {
    /// Locks consolidated controller state, recovering after an internal panic.
    fn state(&self) -> StdMutexGuard<'_, ControllerState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Clones a slot reference without keeping the controller-state lock.
    fn slot(&self, name: &str) -> Option<Arc<Mutex<SlotState>>> {
        self.state().slots.get(name).cloned()
    }

    /// Creates a controller and its bounded ordered command channel.
    ///
    /// The controller is inert until [`run`](Self::run) is called.
    pub fn new(config: ControllerConfig, supervisor: &Arc<SupervisorCore>, bus: Bus) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(config.queue_capacity().get());
        let shutdown_token = supervisor.shutdown_started_token();

        Arc::new(Self {
            config,
            supervisor: Arc::downgrade(supervisor),
            drop_domain: supervisor.drop_domain().clone(),
            bus,
            shutdown_token,
            state: StdMutex::new(ControllerState::default()),
            tx,
            rx: RwLock::new(Some(rx)),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            task: OnceLock::new(),
        })
    }

    /// Resolves a parked watched submission as `Rejected`.
    ///
    /// This is a no-op for unwatched submissions and for watched submissions already handed to the runtime registry.
    fn finalize_rejected(
        &self,
        id: TaskId,
        kind: RejectionKind,
        reason: &str,
    ) -> Option<TaskOutcome> {
        let tx = self.state().watchers.remove(&id)?;
        Self::send_rejected(Some(tx), kind, reason)
    }

    /// Sends a controller rejection and returns it if the receiver is gone.
    fn send_rejected(
        done: Option<OutcomeTx>,
        kind: RejectionKind,
        reason: &str,
    ) -> Option<TaskOutcome> {
        done?
            .send(TaskOutcome::Rejected {
                kind,
                reason: Arc::from(reason),
            })
            .err()
    }

    /// Marks the controller as no longer admitting or advancing queued work.
    ///
    /// The command receiver closes later during shutdown drain.
    /// Commands that enter before then are rejected or resolved by that drain.
    fn mark_shutting_down(&self) {
        self.shutting_down
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Returns `true` when the shutdown signal has fired or the loop set its local shutdown flag.
    fn is_shutting_down(&self) -> bool {
        self.shutdown_token.is_cancelled()
            || self
                .shutting_down
                .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Rejects any watcher retained after normal or abnormal loop exit.
    fn finalize_remaining_watchers(&self) {
        let pending: Vec<TaskId> = self.state().watchers.keys().copied().collect();
        for id in pending {
            self.bus.publish_lazy(|| {
                Event::new(EventKind::ControllerRejected)
                    .with_id(id)
                    .with_rejection_kind(RejectionKind::ControllerShuttingDown)
                    .with_reason(crate::reasons::CONTROLLER_SHUTTING_DOWN)
            });
            drop(self.finalize_rejected(
                id,
                RejectionKind::ControllerShuttingDown,
                crate::reasons::CONTROLLER_SHUTTING_DOWN,
            ));
        }
    }

    /// Returns a cloneable client for the ordered controller command queue.
    pub fn handle(&self) -> ControllerHandle {
        ControllerHandle::new(self.tx.clone(), self.bus.clone(), self.drop_domain.clone())
    }
}

#[cfg(test)]
mod tests;
