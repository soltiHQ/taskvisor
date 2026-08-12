//! Internal controller engine.
//!
//! The controller is the slot-based admission layer behind `SupervisorHandle::submit`, `try_submit`, and `submit_and_watch`.
//!
//! It owns:
//!
//! - watched submission senders until they are handed to the runtime or rejected,
//! - task payloads waiting for transient registry command capacity,
//! - the bounded ordered channel for submissions and identity operations,
//! - the per-slot state map,
//! - a reverse index from every queued [`TaskId`] to its slot.
//!
//! ## Authoritative slot-state inputs
//!
//! One controller loop applies slot ownership and queue transitions.
//! These authoritative inputs are separate from the best-effort event path:
//!
//! ```text
//! Ordered controller commands ────────┐
//! Direct registry Add decisions ──────┤
//! Terminal registry completions ──────┼──► controller loop ───► slot state
//! Runtime shutdown-start signal ──────┘
//!
//! controller loop ── Event (best-effort) ──► event bus
//! ```
//!
//! A successful removal request does not release a slot.
//! The controller starts queued work only after logical terminal reporting has
//! removed the previous ID and label and the physical actor/reaper owner has
//! released execution.
//! Task lifecycle events are observability only and never decide slot state.
//!
//! Removal replies and completed identity-operation workers also return to the loop.
//! They may produce diagnostics or caller replies, but they do not release a slot owner.
//!
//! ## Submission Outcomes
//!
//! Unwatched submissions have no final-outcome receiver.
//! Their lifecycle can be observed through best-effort events and aggregate slot snapshots.
//! Watched submissions keep an `OutcomeTx` until one of two things happens:
//! - its Add command is committed and the watcher is handed to the runtime registry,
//! - the submission is rejected and resolved as `TaskOutcome::Rejected`.
//!
//! A submission carries its immutable task name in `TaskSpec`, so the
//! serialized controller loop can resolve the effective slot without an
//! asynchronous metadata stage.
//!
//! ## Internal Architecture
//!
//! `Controller` keeps shared state and construction in this facade.
//! The command-side API lives in `handle`, wire messages in `protocol`, and the serialized transition loop in `lifecycle`.
//!
//! Admission, identity operations, registry worker tracking, and slot queue mechanics live in dedicated workflow modules.
//! Shutdown and introspection are separate read/drain concerns.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard, OnceLock, Weak},
};

use tokio::sync::{Mutex, RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{
    core::{OutcomeTx, SupervisorCore, TaskOutcome},
    events::{Bus, Event, EventKind, RejectionKind},
    identity::TaskId,
};

use super::{
    config::ControllerConfig,
    slot::{PendingSubmission, SlotState},
};

/// Controller-owned admission payload waiting for a reserved registry queue slot.
struct CapacityPending {
    slot_name: Arc<str>,
    pending: PendingSubmission,
}

/// Controller-owned indexes changed by one serialized transition loop.
#[derive(Default)]
struct ControllerState {
    slots: HashMap<Arc<str>, Arc<Mutex<SlotState>>>,
    queued_slots: HashMap<TaskId, Arc<str>>,
    capacity_pending: HashMap<TaskId, CapacityPending>,
    watchers: HashMap<TaskId, OutcomeTx>,
}

impl ControllerState {
    fn pending_len(&self) -> usize {
        self.queued_slots.len() + self.capacity_pending.len()
    }
}

mod protocol;
use protocol::{
    AdmissionResult, CompletionResult, ControllerCommand, IdentityOperation, IdentityReply,
    RemovalResult, Submission,
};

mod handle;
pub(crate) use handle::ControllerHandle;

mod task;
use task::ControllerTask;

mod admission;
mod identity;
mod lifecycle;
mod queue;
mod workers;
use workers::ControllerWorkers;
#[cfg(test)]
use workers::WorkerSet;

mod introspect;
mod shutdown;

#[cfg(test)]
use super::{
    error::ControllerError,
    slot::{AdmissionTransition, SlotPhase},
    spec::ControllerSpec,
};
#[cfg(test)]
use crate::RuntimeError;
#[cfg(test)]
use std::future::Future;
#[cfg(test)]
use tokio::{sync::oneshot, time::Instant};

/// Slot-based admission controller.
///
/// Slot ownership and queue state are driven by four authoritative inputs:
/// - submissions and identity operations from its ordered command channel,
/// - direct registry replies for in-flight admission,
/// - shared registry completion signals for admitted slot owners,
/// - the reliable runtime shutdown-start signal.
///
/// Removal replies and identity-worker joins also return to the loop, but do not release a slot.
/// Task lifecycle events such as `TaskAdded`, `TaskAddFailed`, and `TaskRemoved` are observability only and never decide slot state.
pub(crate) struct Controller {
    /// Static controller configuration.
    config: ControllerConfig,
    /// Runtime control surface.
    /// `Weak` avoids extending the runtime core's lifetime during teardown.
    supervisor: Weak<SupervisorCore>,
    /// Runtime event bus used for controller observability and diagnostics.
    bus: Bus,
    /// Reliable signal fired when the runtime's shared shutdown operation starts.
    shutdown_token: CancellationToken,
    /// Per-slot state, reverse indexes, and pre-commit watcher ownership.
    ///
    /// This lock is never held across `.await`, event publication, reply delivery, or user-value
    /// destruction. One lock makes aggregate limits and cross-index updates atomic.
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

    /// Clones one slot reference without extending the controller-state lock into async work.
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

    /// Delivers one controller rejection and returns a terminal value rejected by its receiver.
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
            let undelivered = self.finalize_rejected(
                id,
                RejectionKind::ControllerShuttingDown,
                crate::reasons::CONTROLLER_SHUTTING_DOWN,
            );
            if let Some(outcome) = undelivered {
                // Controller rejection outcomes contain no user-provided source value.
                drop(outcome);
            }
        }
    }

    /// Returns a cloneable handle for sending controller submissions.
    pub fn handle(&self) -> ControllerHandle {
        ControllerHandle::new(self.tx.clone(), self.bus.clone())
    }
}

#[cfg(test)]
mod tests;
