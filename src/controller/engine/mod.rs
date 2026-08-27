//! Internal slot admission engine.
//!
//! One loop owns the ordered command receiver and all controller state transitions.
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

use std::sync::{
    Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard, OnceLock, Weak,
    atomic::{AtomicBool, Ordering},
};

use tokio::sync::{Mutex, mpsc};
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
    /// Runtime control surface that does not extend the core lifetime during teardown.
    supervisor: Weak<SupervisorCore>,
    /// Supervisor-local ownership capacity and background cleanup workers.
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
    rx: StdMutex<Option<mpsc::Receiver<ControllerCommand>>>,
    /// Set when the controller loop begins shutdown or exits.
    shutting_down: AtomicBool,
    /// Single controller loop task shared by every start and join caller.
    task: OnceLock<ControllerTask>,
}

impl Controller {
    /// Poison-recovering access to consolidated controller state.
    fn state(&self) -> StdMutexGuard<'_, ControllerState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Slot access without retaining the controller-state lock.
    fn slot(&self, name: &str) -> Option<Arc<Mutex<SlotState>>> {
        self.state().slots.get(name).cloned()
    }

    /// Bounded controller state with an inert command receiver.
    ///
    /// The controller is inert until [`run`](Self::run) is called.
    pub(crate) fn new(
        config: ControllerConfig,
        supervisor: &Arc<SupervisorCore>,
        bus: Bus,
    ) -> Arc<Self> {
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
            rx: StdMutex::new(Some(rx)),
            shutting_down: AtomicBool::new(false),
            task: OnceLock::new(),
        })
    }

    /// Terminal rejection for a parked watched submission.
    ///
    /// Unwatched submissions and registry-owned watchers are unchanged.
    fn finalize_rejected(
        &self,
        id: TaskId,
        kind: RejectionKind,
        reason: &str,
    ) -> Option<TaskOutcome> {
        let tx = self.state().watchers.remove(&id)?;
        Self::send_rejected(Some(tx), kind, reason)
    }

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

    /// Admission fence before command-queue shutdown drain.
    ///
    /// The command receiver closes later during shutdown drain.
    /// Commands that enter before then are rejected or resolved by that drain.
    fn mark_shutting_down(&self) {
        self.shutting_down.store(true, Ordering::Release);
    }

    fn is_shutting_down(&self) -> bool {
        self.shutdown_token.is_cancelled() || self.shutting_down.load(Ordering::Acquire)
    }

    /// Terminal rejection for every watcher retained after loop exit.
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
    pub(crate) fn handle(&self) -> ControllerHandle {
        ControllerHandle::new(self.tx.clone(), self.bus.clone(), self.drop_domain.clone())
    }
}

impl Drop for Controller {
    /// Closes an inert receiver before it is destroyed.
    fn drop(&mut self) {
        self.mark_shutting_down();
        let rx = self.rx.get_mut().unwrap_or_else(|error| error.into_inner());
        if let Some(rx) = rx.as_mut() {
            rx.close();
        }
    }
}

#[cfg(test)]
mod tests;
