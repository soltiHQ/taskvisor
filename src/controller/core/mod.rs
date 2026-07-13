//! Internal controller engine.
//!
//! The controller is the slot-based admission layer behind `SupervisorHandle::submit`, `try_submit`, and `submit_and_watch`.
//!
//! It owns:
//!
//! - the bounded ordered channel for submissions and identity operations,
//! - the per-slot state map,
//! - watched submission senders until they are handed to the runtime or rejected.
//!
//! ## Authoritative inputs
//!
//! One serialized loop applies all state changes. Its control inputs are
//! separate from the best-effort event path:
//!
//! ```text
//! Ordered controller commands ────────┐
//! Registry add decisions ─────────────┤
//! Terminal registry completions ──────┼──► controller loop ───► slot state
//! Runtime shutdown-start signal ──────┘
//!
//! controller loop ── Event (best-effort) ──► event bus
//! ```
//!
//! A successful removal request does not release a slot. The controller starts
//! queued work only after the terminal completion confirms that the previous
//! actor is joined and its ID and label are removed. Task lifecycle events are
//! observability only and never decide slot state.
//!
//! ## Submission Outcomes
//!
//! Unwatched submissions report progress only through events.
//! Watched submissions keep an `OutcomeTx` until one of two things happens:
//!
//! - the submission is admitted and the watcher is handed to the runtime registry,
//! - the submission is rejected and resolved as `TaskOutcome::Rejected`.
//!
//! ## Internal Architecture
//!
//! `Controller` keeps shared state and construction in this facade. The command-side API lives in
//! `handle`, wire messages in `protocol`, and the single serialized actor loop in `lifecycle`.
//! Admission, identity operations, registry worker tracking, and slot queue mechanics are isolated
//! in dedicated workflow modules. Shutdown and introspection remain separate read/drain concerns.

use std::sync::{Arc, OnceLock, Weak};

use dashmap::DashMap;
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{
    core::{OutcomeTx, SupervisorCore, TaskOutcome},
    events::{Bus, Event, EventKind},
    identity::TaskId,
};

use super::{config::ControllerConfig, slot::SlotState};

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
use tokio::{sync::oneshot, task::JoinSet, time::Instant};

/// Slot-based admission controller.
///
/// The controller is driven by four inputs:
/// - submissions and identity operations from its ordered command channel,
/// - direct registry replies for in-flight admission,
/// - shared registry completion signals for admitted slot owners,
/// - the reliable runtime shutdown-start signal.
///
/// Task lifecycle events such as `TaskAdded`, `TaskAddFailed`, and `TaskRemoved` are observability
/// only and never decide slot state.
pub(crate) struct Controller {
    /// Static controller configuration.
    config: ControllerConfig,
    /// Runtime control surface. Weak avoids an ownership cycle with the supervisor.
    supervisor: Weak<SupervisorCore>,
    /// Runtime event bus used for controller observability and diagnostics.
    bus: Bus,
    /// Reliable signal fired when the runtime's shared shutdown operation starts.
    shutdown_token: CancellationToken,
    /// Per-slot mutable state.
    slots: DashMap<Arc<str>, Arc<Mutex<SlotState>>>,
    /// Watched submissions not yet handed to the runtime registry.
    watchers: DashMap<TaskId, OutcomeTx>,
    /// Ordered command sender cloned into `ControllerHandle`.
    tx: mpsc::Sender<ControllerCommand>,
    /// Single-use command receiver owned by the controller loop.
    rx: RwLock<Option<mpsc::Receiver<ControllerCommand>>>,
    /// Set after the reliable runtime shutdown signal is observed.
    shutting_down: std::sync::atomic::AtomicBool,
    /// Single controller loop task shared by every start and join caller.
    task: OnceLock<ControllerTask>,
}

impl Controller {
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
            slots: DashMap::new(),
            watchers: DashMap::new(),
            tx,
            rx: RwLock::new(Some(rx)),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            task: OnceLock::new(),
        })
    }

    /// Resolves a parked watched submission as `Rejected`.
    ///
    /// This is a no-op for unwatched submissions and for watched submissions already handed to the runtime registry.
    fn finalize_rejected(&self, id: TaskId, reason: &str) {
        if let Some((_, tx)) = self.watchers.remove(&id) {
            let _ = tx.send(TaskOutcome::Rejected {
                reason: Arc::from(reason),
            });
        }
    }

    /// Marks the controller as no longer accepting or advancing work.
    fn mark_shutting_down(&self) {
        self.shutting_down
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Returns `true` after the reliable runtime shutdown signal is observed.
    fn is_shutting_down(&self) -> bool {
        self.shutdown_token.is_cancelled()
            || self
                .shutting_down
                .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Rejects any watcher retained after normal or abnormal loop exit.
    fn finalize_remaining_watchers(&self) {
        let pending: Vec<TaskId> = self.watchers.iter().map(|entry| *entry.key()).collect();
        for id in pending {
            self.bus.publish(
                Event::new(EventKind::ControllerRejected)
                    .with_id(id)
                    .with_reason(crate::reasons::CONTROLLER_SHUTTING_DOWN),
            );
            self.finalize_rejected(id, crate::reasons::CONTROLLER_SHUTTING_DOWN);
        }
    }

    /// Returns a cloneable handle for sending controller submissions.
    pub fn handle(&self) -> ControllerHandle {
        ControllerHandle::new(self.tx.clone())
    }
}

#[cfg(test)]
mod tests;
