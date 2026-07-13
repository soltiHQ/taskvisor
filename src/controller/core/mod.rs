//! Internal controller engine.
//!
//! The controller is the slot-based admission layer behind `SupervisorHandle::submit`, `try_submit`, and `submit_and_watch`.
//!
//! It owns:
//!
//! - the bounded ordered channel for submissions and identity operations,
//! - the per-slot state map,
//! - the reverse index from running [`TaskId`] to slot,
//! - watched submission senders until they are handed to the runtime or rejected.
//!
//! ## Per-slot Flow
//!
//! ```text
//! Idle
//!   submit
//!   |
//!   v
//! Admitting -- Add reply Ok --> Running -- registry completion --> Idle or next queued submission
//!   |                           |
//!   | Add reply Err             | Replace
//!   v                           v
//! Idle or next queued         Terminating -- registry completion --> Idle or next queued submission
//! ```
//!
//! The controller advances admission from direct registry replies.
//! It starts queued work only after the registry confirms that the previous actor is joined and
//! its id and label are removed. Lifecycle events are observability only.
//!
//! ## Submission Outcomes
//!
//! Unwatched submissions report progress only through events.
//! Watched submissions keep an `OutcomeTx` until one of two things happens:
//!
//! - the submission is admitted and the watcher is handed to the runtime registry,
//! - the submission is rejected and resolved as `TaskOutcome::Rejected`.

use std::{
    future::Future,
    sync::{Arc, OnceLock, Weak},
};

use dashmap::DashMap;
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::{
    RuntimeError, TaskSpec,
    core::{AddReplyRx, OutcomeTx, RemovalCompletion, SupervisorCore, TaskOutcome},
    events::{Bus, Event, EventKind},
    identity::TaskId,
};

use super::{
    admission::AdmissionPolicy,
    config::ControllerConfig,
    error::ControllerError,
    slot::{SlotState, SlotStatus},
    spec::ControllerSpec,
};

mod introspect;
mod shutdown;

/// Work accepted by the ordered controller command channel.
enum ControllerCommand {
    /// Apply admission policy for one new submission.
    Submit(Submission),
    /// Apply one identity operation after all earlier controller submissions.
    ///
    /// The controller removes queued work itself and owns registry fallback for every other id.
    ManageIdentity {
        id: TaskId,
        operation: IdentityOperation,
        reply: oneshot::Sender<Result<bool, RuntimeError>>,
    },
}

/// Identity operation owned by one accepted controller command.
#[derive(Clone, Copy, Debug)]
enum IdentityOperation {
    Remove,
    TryRemove,
    Cancel,
    CancelWithTimeout(std::time::Duration),
}

impl IdentityOperation {
    /// Optional reason carried by `TaskRemoveRequested` when queued work is removed.
    fn request_reason(self) -> Option<&'static str> {
        match self {
            Self::Remove | Self::TryRemove => None,
            Self::Cancel | Self::CancelWithTimeout(_) => Some("manual_cancel"),
        }
    }
}

/// Ensures an accepted identity caller receives an explicit shutdown result if its worker aborts.
struct IdentityReply {
    sender: Option<oneshot::Sender<Result<bool, RuntimeError>>>,
}

impl IdentityReply {
    fn new(sender: oneshot::Sender<Result<bool, RuntimeError>>) -> Self {
        Self {
            sender: Some(sender),
        }
    }

    fn send(mut self, result: Result<bool, RuntimeError>) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(result);
        }
    }
}

impl Drop for IdentityReply {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(Err(RuntimeError::ShuttingDown));
        }
    }
}

/// Submission accepted by the controller command channel.
struct Submission {
    /// Pre-minted identity used for events, slot state, and final outcome correlation.
    id: TaskId,
    /// Admission policy, task spec, and optional slot key.
    spec: ControllerSpec,
    /// Optional watched-outcome sender for `submit_and_watch`.
    done: Option<OutcomeTx>,
}

/// Authoritative registry decision for one in-flight slot admission.
struct AdmissionResult {
    /// Pre-minted identity used to reject stale results safely.
    id: TaskId,
    /// Slot that owned `id` when the Add command was committed.
    slot_name: Arc<str>,
    /// Direct registry decision and the terminal signal for an accepted task.
    decision: Result<RemovalCompletion, RuntimeError>,
}

/// Reliable registry cleanup for one admitted slot owner.
struct CompletionResult {
    /// Runtime identity that reached terminal registry cleanup.
    id: TaskId,
    /// Slot that owned `id` when completion tracking started.
    slot_name: Arc<str>,
}

/// Result of ordering removal for one controller-owned runtime task.
struct RemovalResult {
    /// Runtime identity whose removal was requested.
    id: TaskId,
    /// Slot that owned `id` when removal tracking started.
    slot_name: Arc<str>,
    /// Direct registry claim decision or management-plane failure.
    decision: Result<bool, RuntimeError>,
}

/// One shared controller loop task with a cancellation-safe, idempotent join.
struct ControllerTask {
    state: Mutex<ControllerTaskState>,
}

enum ControllerTaskState {
    Running(JoinHandle<()>),
    Joined(bool),
}

impl ControllerTask {
    fn new(handle: JoinHandle<()>) -> Self {
        Self {
            state: Mutex::new(ControllerTaskState::Running(handle)),
        }
    }

    /// Joins the stored task without taking it out of shared state before the await.
    ///
    /// If this future is dropped, a later caller can continue polling the same `JoinHandle`.
    /// Returns `false` when Tokio reports that the controller task did not join cleanly.
    async fn join(&self, bus: &Bus) -> bool {
        let mut state = self.state.lock().await;
        if let ControllerTaskState::Joined(clean) = &*state {
            return *clean;
        }
        let ControllerTaskState::Running(handle) = &mut *state else {
            unreachable!("joined controller state was returned above")
        };

        let clean = match handle.await {
            Ok(()) => true,
            Err(error) => {
                bus.publish(
                    Event::new(EventKind::ControllerRejected)
                        .with_task("controller")
                        .with_reason(format!("controller_join_failed: {error}")),
                );
                false
            }
        };
        *state = ControllerTaskState::Joined(clean);
        clean
    }

    #[cfg(test)]
    async fn is_joined(&self) -> bool {
        matches!(*self.state.lock().await, ControllerTaskState::Joined(_))
    }
}

/// Internal handle used by `SupervisorHandle` to send ordered controller commands.
#[derive(Clone)]
pub(crate) struct ControllerHandle {
    tx: mpsc::Sender<ControllerCommand>,
}

impl ControllerHandle {
    /// Sends a submission to the ordered controller command channel.
    ///
    /// This waits for command-channel capacity.
    /// `Ok(id)` means the controller received the submission.
    /// It does not mean the task has been admitted to the runtime yet.
    pub async fn submit(&self, spec: ControllerSpec) -> Result<TaskId, ControllerError> {
        let id = TaskId::next();
        self.tx
            .send(ControllerCommand::Submit(Submission {
                id,
                spec,
                done: None,
            }))
            .await
            .map_err(|_| ControllerError::Closed)?;
        Ok(id)
    }

    /// Tries to send a submission without waiting for command-channel capacity.
    ///
    /// `ControllerError::Full` means the controller command channel is full.
    /// It does not mean the target slot queue is full.
    pub fn try_submit(&self, spec: ControllerSpec) -> Result<TaskId, ControllerError> {
        let id = TaskId::next();
        self.tx
            .try_send(ControllerCommand::Submit(Submission {
                id,
                spec,
                done: None,
            }))
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => ControllerError::Full,
                mpsc::error::TrySendError::Closed(_) => ControllerError::Closed,
            })?;
        Ok(id)
    }

    /// Sends a watched submission to the ordered controller command channel.
    ///
    /// The returned receiver resolves to:
    /// - `TaskOutcome::Rejected` if the controller never admits the task body,
    /// - the runtime task outcome if the task is admitted and later terminates.
    ///
    /// `Ok((id, rx))` means the controller received the submission.
    /// It does not mean the slot accepted it yet.
    pub async fn submit_and_watch(
        &self,
        spec: ControllerSpec,
    ) -> Result<(TaskId, oneshot::Receiver<TaskOutcome>), ControllerError> {
        let id = TaskId::next();
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ControllerCommand::Submit(Submission {
                id,
                spec,
                done: Some(tx),
            }))
            .await
            .map_err(|_| ControllerError::Closed)?;
        Ok((id, rx))
    }

    /// Sends one waiting identity operation to the ordered controller command channel.
    async fn manage_identity(
        &self,
        id: TaskId,
        operation: IdentityOperation,
    ) -> Result<bool, RuntimeError> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .send(ControllerCommand::ManageIdentity {
                id,
                operation,
                reply,
            })
            .await
            .map_err(|_| RuntimeError::ShuttingDown)?;
        reply_rx.await.map_err(|_| RuntimeError::ShuttingDown)?
    }

    /// Sends one fail-fast identity operation to the ordered controller command channel.
    async fn try_manage_identity(
        &self,
        id: TaskId,
        operation: IdentityOperation,
    ) -> Result<bool, RuntimeError> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .try_send(ControllerCommand::ManageIdentity {
                id,
                operation,
                reply,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => RuntimeError::CommandQueueFull,
                mpsc::error::TrySendError::Closed(_) => RuntimeError::ShuttingDown,
            })?;
        reply_rx.await.map_err(|_| RuntimeError::ShuttingDown)?
    }

    pub(crate) async fn remove(&self, id: TaskId) -> Result<bool, RuntimeError> {
        self.manage_identity(id, IdentityOperation::Remove).await
    }

    pub(crate) async fn try_remove(&self, id: TaskId) -> Result<bool, RuntimeError> {
        self.try_manage_identity(id, IdentityOperation::TryRemove)
            .await
    }

    pub(crate) async fn cancel(&self, id: TaskId) -> Result<bool, RuntimeError> {
        self.manage_identity(id, IdentityOperation::Cancel).await
    }

    pub(crate) async fn cancel_with_timeout(
        &self,
        id: TaskId,
        wait_for: std::time::Duration,
    ) -> Result<bool, RuntimeError> {
        self.manage_identity(id, IdentityOperation::CancelWithTimeout(wait_for))
            .await
    }
}

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
    /// Reverse index from current runtime task id to slot name.
    running: DashMap<TaskId, Arc<str>>,
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
            running: DashMap::new(),
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
        ControllerHandle {
            tx: self.tx.clone(),
        }
    }

    /// Starts the single owned controller loop.
    ///
    /// Later calls are no-ops. Runtime shutdown joins this exact task before cleanup completes.
    pub fn run(self: &Arc<Self>) {
        self.task.get_or_init(|| {
            let controller = Arc::clone(self);
            ControllerTask::new(tokio::spawn(async move {
                controller.run_task().await;
            }))
        });
    }

    /// Runs the controller loop behind its outer panic boundary and final state cleanup.
    async fn run_task(self: Arc<Self>) {
        let token = self.shutdown_token.clone();
        match crate::core::panic_guard::guarded(self.run_inner(token)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                self.bus.publish(
                    Event::new(EventKind::ControllerRejected)
                        .with_task("controller")
                        .with_reason(format!("controller_loop_exited: {error}")),
                );
            }
            Err(panic) => {
                self.bus.publish(
                    Event::new(EventKind::ControllerRejected)
                        .with_task("controller")
                        .with_reason(format!("controller_loop_panicked: {panic}")),
                );
            }
        }

        self.mark_shutting_down();
        self.finalize_remaining_watchers();
        self.slots.clear();
        self.running.clear();
    }

    /// Waits for the owned controller loop exactly once.
    ///
    /// Concurrent and later callers share the stored join state.
    /// Returns `false` when the controller task did not join cleanly.
    pub(crate) async fn join(&self) -> bool {
        if let Some(task) = self.task.get() {
            task.join(&self.bus).await
        } else {
            true
        }
    }

    #[cfg(test)]
    async fn is_joined(&self) -> bool {
        match self.task.get() {
            Some(task) => task.is_joined().await,
            None => false,
        }
    }

    /// Runs the controller event loop.
    ///
    /// The loop receives ordered controller commands, registry admission/completion results, and
    /// the reliable runtime shutdown-start signal.
    ///
    /// On shutdown, it closes the command receiver, drains buffered commands, and resolves pending
    /// submissions and removal replies.
    async fn run_inner(&self, token: CancellationToken) -> Result<(), ControllerError> {
        let mut rx = self
            .rx
            .write()
            .await
            .take()
            .ok_or(ControllerError::AlreadyStarted)?;

        let mut admissions = JoinSet::new();
        let mut completions = JoinSet::new();
        let mut removals = JoinSet::new();
        let mut identity_operations = JoinSet::new();
        let identity_operation_limit = self.config.queue_capacity().get();
        let loop_result = crate::core::panic_guard::guarded(async {
            loop {
                tokio::select! {
                biased;

                _ = token.cancelled() => {
                    self.mark_shutting_down();
                    break;
                },

                Some(command) = rx.recv(), if identity_operations.len() < identity_operation_limit => {
                    match command {
                        ControllerCommand::Submit(sub) => {
                            let _ = self
                                .guarded(
                                    "handle_submission",
                                    self.handle_submission(sub, &mut admissions, &mut removals),
                                )
                                .await;
                        }
                        ControllerCommand::ManageIdentity {
                            id,
                            operation,
                            reply,
                        } => {
                            let _ = self
                                .guarded(
                                    "handle_identity_operation",
                                    self.handle_identity_operation(
                                        id,
                                        operation,
                                        reply,
                                        &mut identity_operations,
                                    ),
                                )
                                .await;
                        }
                    }
                }
                result = admissions.join_next(), if !admissions.is_empty() => {
                    match result {
                        Some(Ok(result)) => {
                            let _ = self
                                .guarded(
                                    "handle_admission_result",
                                    self.handle_admission_result(
                                        result,
                                        &mut admissions,
                                        &mut completions,
                                        &mut removals,
                                    ),
                                )
                                .await;
                        }
                        Some(Err(error)) => {
                            self.bus.publish(
                                Event::new(EventKind::ControllerRejected)
                                    .with_task("controller")
                                    .with_reason(format!("admission_waiter_failed: {error}")),
                            );
                        }
                        None => {}
                    }
                }
                result = completions.join_next(), if !completions.is_empty() => {
                    match result {
                        Some(Ok(result)) => {
                            let _ = self
                                .guarded(
                                    "handle_completion_result",
                                    self.handle_completion_result(result, &mut admissions),
                                )
                                .await;
                        }
                        Some(Err(error)) => {
                            self.bus.publish(
                                Event::new(EventKind::ControllerRejected)
                                    .with_task("controller")
                                    .with_reason(format!("completion_waiter_failed: {error}")),
                            );
                        }
                        None => {}
                    }
                }
                result = removals.join_next(), if !removals.is_empty() => {
                    match result {
                        Some(Ok(result)) => self.handle_removal_result(result),
                        Some(Err(error)) => {
                            self.bus.publish(
                                Event::new(EventKind::ControllerRejected)
                                    .with_task("controller")
                                    .with_reason(format!("removal_waiter_failed: {error}")),
                            );
                        }
                        None => {}
                    }
                }
                result = identity_operations.join_next(), if !identity_operations.is_empty() => {
                    match result {
                        Some(Ok(())) => {}
                        Some(Err(error)) => {
                            self.bus.publish(
                                Event::new(EventKind::ControllerRejected)
                                    .with_task("controller")
                                    .with_reason(format!("identity_operation_failed: {error}")),
                            );
                        }
                        None => {}
                    }
                }
                }
            }
        })
        .await;
        self.finalize_pending_on_shutdown(&mut rx);
        admissions.abort_all();
        completions.abort_all();
        removals.abort_all();
        identity_operations.abort_all();
        Self::drain_workers(&mut admissions).await;
        Self::drain_workers(&mut completions).await;
        Self::drain_workers(&mut removals).await;
        Self::drain_workers(&mut identity_operations).await;
        self.finalize_slot_state_on_shutdown().await;
        if let Err(panic) = loop_result {
            self.bus.publish(
                Event::new(EventKind::ControllerRejected)
                    .with_task("controller")
                    .with_reason(format!("controller_loop_panicked: {panic}")),
            );
        }
        Ok(())
    }

    /// Runs one controller work unit behind a panic boundary.
    ///
    /// A panic is converted into a diagnostic `ControllerRejected` event and the loop continues.
    ///
    /// This guard does not repair partially updated slot state by itself.
    /// Callers that park watcher state must still make sure the watcher is resolved or returned on every failure path.
    /// Returns the work-unit output on success and `None` after a caught panic.
    async fn guarded<T>(&self, who: &'static str, fut: impl Future<Output = T>) -> Option<T> {
        match crate::core::panic_guard::guarded(fut).await {
            Ok(output) => Some(output),
            Err(msg) => {
                self.bus.publish(
                    Event::new(EventKind::ControllerRejected)
                        .with_task("controller")
                        .with_reason(format!("{who}_panicked: {msg}")),
                );
                None
            }
        }
    }

    /// Applies admission policy for one submission.
    ///
    /// Watched submissions are parked in `watchers` until they are either rejected by the controller or handed to the runtime registry.
    ///
    /// Policy behavior:
    /// - idle slot: admit immediately and enter `Admitting`,
    /// - busy + `Queue`: append to the slot queue, unless the queue is full,
    /// - busy + `Replace`: retire the current owner if needed and keep this
    ///   submission as the next queued owner,
    /// - busy + `DropIfRunning`: reject immediately.
    ///
    /// A slot becomes `Running` only after the direct registry Add reply succeeds.
    async fn handle_submission(
        &self,
        sub: Submission,
        admissions: &mut JoinSet<AdmissionResult>,
        removals: &mut JoinSet<RemovalResult>,
    ) {
        let Some(sup) = self.supervisor.upgrade() else {
            return;
        };
        let Submission { id, spec, done } = sub;
        if let Some(tx) = done {
            self.watchers.insert(id, tx);
        }
        if self.is_shutting_down() {
            self.bus.publish(
                Event::new(EventKind::ControllerRejected)
                    .with_task(spec.slot_name().to_owned())
                    .with_id(id)
                    .with_reason(crate::reasons::CONTROLLER_SHUTTING_DOWN),
            );
            self.finalize_rejected(id, crate::reasons::CONTROLLER_SHUTTING_DOWN);
            return;
        }

        let slot_name: Arc<str> = Arc::from(spec.slot_name());
        let admission = spec.admission;
        let task_spec = spec.task_spec;

        let slot_arc = self.get_or_create_slot(&slot_name);
        let mut slot = slot_arc.lock().await;
        if self.is_shutting_down() {
            self.bus.publish(
                Event::new(EventKind::ControllerRejected)
                    .with_task(Arc::clone(&slot_name))
                    .with_id(id)
                    .with_reason(crate::reasons::CONTROLLER_SHUTTING_DOWN),
            );
            self.finalize_rejected(id, crate::reasons::CONTROLLER_SHUTTING_DOWN);
            return;
        }

        match (&slot.status, admission) {
            (SlotStatus::Idle, _) => {
                match self.start_in_slot(&sup, &mut slot, &slot_name, id, task_spec, admissions) {
                    Ok(()) => {
                        let reason: &'static str = match admission {
                            AdmissionPolicy::Queue => "admission=Queue status=admitting",
                            AdmissionPolicy::Replace => "admission=Replace status=admitting",
                            AdmissionPolicy::DropIfRunning => {
                                "admission=DropIfRunning status=admitting"
                            }
                        };
                        self.bus.publish(
                            Event::new(EventKind::ControllerSubmitted)
                                .with_task(Arc::clone(&slot_name))
                                .with_id(id)
                                .with_reason(reason),
                        );
                    }
                    Err(e) => {
                        let reason = format!("add_failed: {e}");
                        self.bus.publish(
                            Event::new(EventKind::ControllerRejected)
                                .with_task(Arc::clone(&slot_name))
                                .with_id(id)
                                .with_reason(reason.clone()),
                        );
                        self.finalize_rejected(id, &reason);
                        self.gc_if_idle(&slot_name, slot);
                    }
                }
            }
            (SlotStatus::Running { .. }, AdmissionPolicy::Replace) => {
                self.replace_head_or_push(&mut slot, &slot_name, id, task_spec);
                slot.status = SlotStatus::Terminating {
                    cancelled_at: Instant::now(),
                };
                if let Some(rid) = slot.running_id {
                    Self::track_removal(removals, Arc::clone(&sup), rid, Arc::clone(&slot_name));
                }
                self.bus.publish(
                    Event::new(EventKind::ControllerSlotTransition)
                        .with_task(Arc::clone(&slot_name))
                        .with_reason("running→terminating (replace)"),
                );
                self.bus.publish(
                    Event::new(EventKind::ControllerSubmitted)
                        .with_task(Arc::clone(&slot_name))
                        .with_id(id)
                        .with_reason(format!("admission=Replace depth={}", slot.queue.len())),
                );
            }
            (SlotStatus::Admitting { .. }, AdmissionPolicy::Replace) => {
                self.replace_head_or_push(&mut slot, &slot_name, id, task_spec);
                slot.status = SlotStatus::Terminating {
                    cancelled_at: Instant::now(),
                };
                self.bus.publish(
                    Event::new(EventKind::ControllerSlotTransition)
                        .with_task(Arc::clone(&slot_name))
                        .with_reason("admitting→terminating (replace)"),
                );
                self.bus.publish(
                    Event::new(EventKind::ControllerSubmitted)
                        .with_task(Arc::clone(&slot_name))
                        .with_id(id)
                        .with_reason(format!(
                            "admission=Replace status=admitting depth={}",
                            slot.queue.len()
                        )),
                );
            }
            (SlotStatus::Terminating { .. }, AdmissionPolicy::Replace) => {
                self.replace_head_or_push(&mut slot, &slot_name, id, task_spec);
                self.bus.publish(
                    Event::new(EventKind::ControllerSubmitted)
                        .with_task(Arc::clone(&slot_name))
                        .with_id(id)
                        .with_reason(format!(
                            "admission=Replace status=terminating depth={}",
                            slot.queue.len()
                        )),
                );
            }
            (
                SlotStatus::Admitting { .. }
                | SlotStatus::Running { .. }
                | SlotStatus::Terminating { .. },
                AdmissionPolicy::Queue,
            ) => {
                if self.reject_if_full(&slot_name, id, slot.queue.len()) {
                    return;
                }
                slot.queue.push_back((id, task_spec));
                self.bus.publish(
                    Event::new(EventKind::ControllerSubmitted)
                        .with_task(Arc::clone(&slot_name))
                        .with_id(id)
                        .with_reason(format!("admission=Queue depth={}", slot.queue.len())),
                );
            }
            (
                SlotStatus::Admitting { .. }
                | SlotStatus::Running { .. }
                | SlotStatus::Terminating { .. },
                AdmissionPolicy::DropIfRunning,
            ) => {
                let reason = format!("dropped: slot busy ({})", slot.status.label());
                self.bus.publish(
                    Event::new(EventKind::ControllerRejected)
                        .with_task(Arc::clone(&slot_name))
                        .with_id(id)
                        .with_reason(reason.clone()),
                );
                self.finalize_rejected(id, &reason);
            }
        }
    }

    /// Applies one accepted identity operation after all earlier controller commands.
    ///
    /// The controller claims still-queued work directly. For every other id, it owns the registry
    /// fallback in a tracked task so dropping the public caller cannot stop an accepted operation.
    async fn handle_identity_operation(
        &self,
        id: TaskId,
        operation: IdentityOperation,
        reply: oneshot::Sender<Result<bool, RuntimeError>>,
        identity_operations: &mut JoinSet<()>,
    ) {
        let reply = IdentityReply::new(reply);
        if self.is_shutting_down() {
            reply.send(Err(RuntimeError::ShuttingDown));
            return;
        }
        if self
            .remove_queued_submission(id, operation.request_reason())
            .await
        {
            reply.send(Ok(true));
            return;
        }
        if self.is_shutting_down() {
            reply.send(Err(RuntimeError::ShuttingDown));
            return;
        }

        let Some(supervisor) = self.supervisor.upgrade() else {
            reply.send(Err(RuntimeError::ShuttingDown));
            return;
        };

        identity_operations.spawn(async move {
            let result = match operation {
                IdentityOperation::Remove => supervisor.remove(id).await,
                IdentityOperation::TryRemove => supervisor.try_remove(id).await,
                IdentityOperation::Cancel => supervisor.cancel(id).await,
                IdentityOperation::CancelWithTimeout(wait_for) => {
                    supervisor.cancel_with_timeout(id, wait_for).await
                }
            };
            reply.send(result);
        });
    }

    /// Removes one queued, not-yet-admitted submission by identity.
    ///
    /// Returns `true` only when this call claimed the queued submission. A claimed watched
    /// submission resolves as `Rejected("removed_from_queue")` because its task body never ran.
    async fn remove_queued_submission(
        &self,
        id: TaskId,
        request_reason: Option<&'static str>,
    ) -> bool {
        let slot_keys: Vec<Arc<str>> = self
            .slots
            .iter()
            .map(|entry| Arc::clone(entry.key()))
            .collect();

        for slot_name in slot_keys {
            let Some(slot_arc) = self.slots.get(&*slot_name).map(|e| e.clone()) else {
                continue;
            };
            let mut slot = slot_arc.lock().await;
            if self.is_shutting_down() {
                return false;
            }
            let Some(pos) = slot.queue.iter().position(|(qid, _)| *qid == id) else {
                continue;
            };
            let task_name: Arc<str> = Arc::from(slot.queue[pos].1.name());
            let mut request = Event::new(EventKind::TaskRemoveRequested)
                .with_task(task_name)
                .with_id(id);
            if let Some(reason) = request_reason {
                request = request.with_reason(reason);
            }
            self.bus.publish(request);
            drop(
                slot.queue
                    .remove(pos)
                    .expect("the queued submission position was checked above"),
            );
            self.bus.publish(
                Event::new(EventKind::ControllerRejected)
                    .with_task(Arc::clone(&slot_name))
                    .with_id(id)
                    .with_reason(crate::reasons::REMOVED_FROM_QUEUE),
            );
            self.finalize_rejected(id, crate::reasons::REMOVED_FROM_QUEUE);
            self.gc_if_idle(&slot_name, slot);
            return true;
        }
        false
    }

    /// Applies one authoritative registry registration decision.
    ///
    /// Correlation by both slot and [`TaskId`] makes a late result harmless after a fast task
    /// has already completed or a different owner has entered the slot.
    async fn handle_admission_result(
        &self,
        result: AdmissionResult,
        admissions: &mut JoinSet<AdmissionResult>,
        completions: &mut JoinSet<CompletionResult>,
        removals: &mut JoinSet<RemovalResult>,
    ) {
        let AdmissionResult {
            id,
            slot_name,
            decision,
        } = result;
        let Some(indexed_slot) = self.running.get(&id).map(|entry| entry.clone()) else {
            return;
        };
        if indexed_slot.as_ref() != slot_name.as_ref() {
            return;
        }
        let Some(slot_arc) = self.slots.get(&*slot_name).map(|entry| entry.clone()) else {
            return;
        };
        let mut slot = slot_arc.lock().await;
        if slot.running_id != Some(id) {
            return;
        }

        match decision {
            Ok(completion) => {
                Self::track_completion(completions, id, Arc::clone(&slot_name), completion);
                match slot.status {
                    SlotStatus::Admitting { .. } => {
                        slot.status = SlotStatus::Running {
                            started_at: Instant::now(),
                        };
                        self.bus.publish(
                            Event::new(EventKind::ControllerSlotTransition)
                                .with_task(slot_name)
                                .with_reason("admitting→running"),
                        );
                    }
                    SlotStatus::Terminating { .. } => {
                        let Some(sup) = self.supervisor.upgrade() else {
                            return;
                        };
                        Self::track_removal(removals, sup, id, slot_name);
                    }
                    SlotStatus::Idle | SlotStatus::Running { .. } => {}
                }
            }
            Err(_) => {
                self.running.remove(&id);
                slot.running_id = None;
                slot.status = SlotStatus::Idle;

                // After commit, the registry owns any watched outcome and lifecycle diagnostics.
                // The direct reply only drives controller state here.

                if !self.is_shutting_down()
                    && let Some(sup) = self.supervisor.upgrade()
                {
                    self.start_next_from_queue(&sup, &mut slot, &slot_name, admissions);
                }

                self.gc_if_idle(&slot_name, slot);
            }
        }
    }

    /// Applies one reliable terminal registry cleanup signal.
    ///
    /// Correlation by slot and [`TaskId`] makes stale or duplicate completions harmless no-ops.
    async fn handle_completion_result(
        &self,
        result: CompletionResult,
        admissions: &mut JoinSet<AdmissionResult>,
    ) {
        let Some(indexed_slot) = self.running.get(&result.id).map(|entry| entry.clone()) else {
            return;
        };
        if indexed_slot.as_ref() != result.slot_name.as_ref() {
            return;
        }
        self.free_and_advance(result.id, admissions).await;
    }

    /// Frees a slot after terminal registry cleanup is confirmed.
    ///
    /// The completed id must match the current slot owner.
    /// If it does, the slot is reset to `Idle` and, unless shutdown is active, the next queued submission is started.
    async fn free_and_advance(&self, id: TaskId, admissions: &mut JoinSet<AdmissionResult>) {
        let Some(sup) = self.supervisor.upgrade() else {
            return;
        };
        let Some(slot_name) = self.running.get(&id).map(|entry| entry.clone()) else {
            return;
        };
        let Some(slot_arc) = self.slots.get(&*slot_name).map(|e| e.clone()) else {
            return;
        };
        let mut slot = slot_arc.lock().await;
        if slot.running_id != Some(id) {
            return;
        }

        self.running.remove(&id);
        slot.running_id = None;
        slot.status = SlotStatus::Idle;

        if !self.is_shutting_down() {
            self.start_next_from_queue(&sup, &mut slot, &slot_name, admissions);
        }

        self.gc_if_idle(&slot_name, slot);
    }

    /// Hands a submission to the runtime under its pre-minted id.
    ///
    /// On success, the slot enters `Admitting`, `running` is updated, and the watcher is owned by the runtime registry.
    ///
    /// On failure, the watcher is put back into `watchers` so the caller can reject it normally instead of dropping the oneshot.
    fn start_in_slot(
        &self,
        sup: &Arc<SupervisorCore>,
        slot: &mut SlotState,
        slot_name: &Arc<str>,
        id: TaskId,
        task_spec: TaskSpec,
        admissions: &mut JoinSet<AdmissionResult>,
    ) -> Result<(), RuntimeError> {
        let done = self.watchers.remove(&id).map(|(_, tx)| tx);
        match sup.add_task_with_id_watched(id, task_spec, done) {
            Ok((reply, completion)) => {
                slot.status = SlotStatus::Admitting {
                    since: Instant::now(),
                };
                slot.running_id = Some(id);
                self.running.insert(id, Arc::clone(slot_name));
                Self::track_admission(admissions, id, Arc::clone(slot_name), reply, completion);
                Ok(())
            }
            Err((e, done)) => {
                if let Some(tx) = done {
                    self.watchers.insert(id, tx);
                }
                Err(e)
            }
        }
    }

    /// Tracks a committed Add command until its direct registry reply arrives.
    fn track_admission(
        admissions: &mut JoinSet<AdmissionResult>,
        id: TaskId,
        slot_name: Arc<str>,
        reply: AddReplyRx,
        completion: RemovalCompletion,
    ) {
        admissions.spawn(async move {
            let decision = match reply.await {
                Ok(Ok(())) => Ok(completion),
                Ok(Err(error)) => Err(error),
                Err(_) => Err(RuntimeError::ShuttingDown),
            };
            AdmissionResult {
                id,
                slot_name,
                decision,
            }
        });
    }

    /// Tracks one accepted task until terminal registry cleanup is committed.
    fn track_completion(
        completions: &mut JoinSet<CompletionResult>,
        id: TaskId,
        slot_name: Arc<str>,
        completion: RemovalCompletion,
    ) {
        completions.spawn(async move {
            completion.wait().await;
            CompletionResult { id, slot_name }
        });
    }

    /// Orders one runtime removal without blocking the controller loop on registry backpressure.
    fn track_removal(
        removals: &mut JoinSet<RemovalResult>,
        supervisor: Arc<SupervisorCore>,
        id: TaskId,
        slot_name: Arc<str>,
    ) {
        removals.spawn(async move {
            let decision = supervisor.remove(id).await;
            RemovalResult {
                id,
                slot_name,
                decision,
            }
        });
    }

    /// Reports a failed removal request without changing slot ownership.
    ///
    /// Successful claims and already-removing tasks both finish through the reliable completion
    /// signal. An error is diagnostic only; shutdown cleanup remains authoritative.
    fn handle_removal_result(&self, result: RemovalResult) {
        let is_current = self
            .running
            .get(&result.id)
            .is_some_and(|slot| slot.as_ref() == result.slot_name.as_ref());
        if !is_current {
            return;
        }
        if let Err(error) = result.decision {
            self.bus.publish(
                Event::new(EventKind::ControllerRejected)
                    .with_task(result.slot_name)
                    .with_id(result.id)
                    .with_reason(format!("remove_failed: {error}")),
            );
        }
    }

    /// Starts the next queued submission, if any.
    ///
    /// Failed starts are rejected and the function continues with the next queued item.
    /// On the first successful start, the slot enters `Admitting`.
    ///
    /// The caller should call this only after the current owner has been cleared.
    fn start_next_from_queue(
        &self,
        sup: &Arc<SupervisorCore>,
        slot: &mut SlotState,
        slot_name: &Arc<str>,
        admissions: &mut JoinSet<AdmissionResult>,
    ) {
        while let Some((next_id, next_spec)) = slot.queue.pop_front() {
            match self.start_in_slot(sup, slot, slot_name, next_id, next_spec, admissions) {
                Ok(()) => {
                    self.bus.publish(
                        Event::new(EventKind::ControllerSubmitted)
                            .with_task(Arc::clone(slot_name))
                            .with_id(next_id)
                            .with_reason(format!("started_from_queue depth={}", slot.queue.len())),
                    );
                    return;
                }
                Err(e) => {
                    let reason = format!("queue_start_failed: {e}");
                    self.bus.publish(
                        Event::new(EventKind::ControllerRejected)
                            .with_task(Arc::clone(slot_name))
                            .with_id(next_id)
                            .with_reason(reason.clone()),
                    );
                    self.finalize_rejected(next_id, &reason);
                }
            }
        }
    }

    /// Returns the slot state for `slot_name`, creating an idle slot when absent.
    #[inline]
    fn get_or_create_slot(&self, slot_name: &str) -> Arc<Mutex<SlotState>> {
        if let Some(slot) = self.slots.get(slot_name) {
            return slot.clone();
        }
        self.slots
            .entry(Arc::from(slot_name))
            .or_insert_with(|| Arc::new(Mutex::new(SlotState::new())))
            .clone()
    }

    /// Removes an idle, empty slot from the slot map.
    ///
    /// The slot lock is released before removing from the map.
    #[inline]
    fn gc_if_idle(&self, slot_name: &Arc<str>, slot: tokio::sync::MutexGuard<'_, SlotState>) {
        let collect = matches!(slot.status, SlotStatus::Idle) && slot.queue.is_empty();
        drop(slot);
        if collect {
            self.slots.remove(&**slot_name);
        }
    }

    /// Rejects a queued submission if the per-slot queue is already full.
    ///
    /// `slot_len` is the current pending queue depth and does not include the current slot owner.
    #[inline]
    fn reject_if_full(&self, slot_name: &str, id: TaskId, slot_len: usize) -> bool {
        if slot_len >= self.config.max_slot_queue() {
            let reason = format!(
                "{}: {}/{}",
                crate::reasons::QUEUE_FULL,
                slot_len,
                self.config.max_slot_queue()
            );
            self.bus.publish(
                Event::new(EventKind::ControllerRejected)
                    .with_task(slot_name)
                    .with_id(id)
                    .with_reason(reason.clone()),
            );
            self.finalize_rejected(id, &reason);
            true
        } else {
            false
        }
    }

    /// Implements latest-wins replacement for queued work.
    ///
    /// If the queue has a head, that head is rejected as `superseded_by_replace` and replaced by the new submission.
    /// If the queue is empty, the new submission becomes the queued head.
    fn replace_head_or_push(
        &self,
        slot: &mut SlotState,
        slot_name: &Arc<str>,
        id: TaskId,
        task_spec: crate::TaskSpec,
    ) {
        if let Some(head) = slot.queue.front_mut() {
            let (displaced_id, _) = std::mem::replace(head, (id, task_spec));
            self.bus.publish(
                Event::new(EventKind::ControllerRejected)
                    .with_task(Arc::clone(slot_name))
                    .with_id(displaced_id)
                    .with_reason(crate::reasons::SUPERSEDED_BY_REPLACE),
            );
            self.finalize_rejected(displaced_id, crate::reasons::SUPERSEDED_BY_REPLACE);
        } else {
            slot.queue.push_front((id, task_spec));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Supervisor;
    use crate::TaskContext;
    use crate::{BackoffPolicy, BoxTaskFuture, RestartPolicy, Task, TaskFn, TaskRef, TaskSpec};
    use std::num::NonZeroUsize;
    use std::sync::{
        Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;

    struct PanickingNameTask;

    impl Task for PanickingNameTask {
        fn name(&self) -> &str {
            panic!("injected task name panic")
        }

        fn spawn(&self, _ctx: TaskContext) -> BoxTaskFuture {
            Box::pin(async { Ok(()) })
        }
    }

    fn make_spec(name: &str) -> TaskSpec {
        let task: TaskRef = TaskFn::arc(name, |_ctx: TaskContext| async { Ok(()) });
        TaskSpec::new(task, RestartPolicy::Never, BackoffPolicy::default(), None)
    }

    fn slot_arc_name() -> Arc<str> {
        Arc::from("s")
    }

    #[test]
    fn replace_head_or_push_into_empty_queue() {
        let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
        let mut slot = SlotState::new();
        ctrl.replace_head_or_push(
            &mut slot,
            &slot_arc_name(),
            TaskId::next(),
            make_spec("first"),
        );

        assert_eq!(slot.queue.len(), 1);
        assert_eq!(slot.queue.front().unwrap().1.name(), "first");
    }

    #[test]
    fn replace_head_or_push_replaces_existing_head_and_rejects_displaced() {
        let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
        let mut rx = ctrl.bus.subscribe();
        let mut slot = SlotState::new();
        let displaced = TaskId::next();
        slot.queue.push_back((displaced, make_spec("old-head")));
        slot.queue.push_back((TaskId::next(), make_spec("tail")));

        ctrl.replace_head_or_push(
            &mut slot,
            &slot_arc_name(),
            TaskId::next(),
            make_spec("new-head"),
        );

        assert_eq!(slot.queue.len(), 2, "queue depth should not grow");
        assert_eq!(slot.queue.front().unwrap().1.name(), "new-head");
        assert_eq!(slot.queue.back().unwrap().1.name(), "tail");

        let ev = rx.try_recv().expect("displaced head must be rejected");
        assert_eq!(ev.kind, EventKind::ControllerRejected);
        assert_eq!(ev.id, Some(displaced));
        assert_eq!(
            ev.reason.as_deref(),
            Some(crate::reasons::SUPERSEDED_BY_REPLACE)
        );
    }

    #[test]
    fn replace_head_multiple_times_keeps_depth_1() {
        let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
        let mut slot = SlotState::new();
        let name = slot_arc_name();
        ctrl.replace_head_or_push(&mut slot, &name, TaskId::next(), make_spec("v1"));
        ctrl.replace_head_or_push(&mut slot, &name, TaskId::next(), make_spec("v2"));
        ctrl.replace_head_or_push(&mut slot, &name, TaskId::next(), make_spec("v3"));

        assert_eq!(slot.queue.len(), 1);
        assert_eq!(slot.queue.front().unwrap().1.name(), "v3");
    }

    #[test]
    fn reject_if_full_returns_false_below_capacity() {
        let bus = Bus::new(64);
        let config = ControllerConfig::new(NonZeroUsize::new(16).unwrap(), 3);
        let ctrl = make_controller(config, bus);
        assert!(!ctrl.reject_if_full("slot", TaskId::next(), 0));
        assert!(!ctrl.reject_if_full("slot", TaskId::next(), 2));
    }

    #[test]
    fn reject_if_full_returns_true_at_capacity() {
        let bus = Bus::new(64);
        let config = ControllerConfig::new(NonZeroUsize::new(16).unwrap(), 3);
        let ctrl = make_controller(config, bus);
        assert!(ctrl.reject_if_full("slot", TaskId::next(), 3));
        assert!(ctrl.reject_if_full("slot", TaskId::next(), 10));
    }

    #[test]
    fn get_or_create_slot_creates_idle_slot() {
        let bus = Bus::new(64);
        let ctrl = make_controller(ControllerConfig::default(), bus);

        let slot_arc = ctrl.get_or_create_slot("my-slot");
        let slot = slot_arc.blocking_lock();
        assert_eq!(slot.status, SlotStatus::Idle);
        assert!(slot.queue.is_empty());
    }

    #[test]
    fn get_or_create_slot_returns_same_arc() {
        let bus = Bus::new(64);
        let ctrl = make_controller(ControllerConfig::default(), bus);

        let s1 = ctrl.get_or_create_slot("x");
        let s2 = ctrl.get_or_create_slot("x");
        assert!(Arc::ptr_eq(&s1, &s2), "same slot name must return same Arc");
    }

    #[test]
    fn get_or_create_slot_different_names_different_arcs() {
        let bus = Bus::new(64);
        let ctrl = make_controller(ControllerConfig::default(), bus);

        let s1 = ctrl.get_or_create_slot("a");
        let s2 = ctrl.get_or_create_slot("b");
        assert!(!Arc::ptr_eq(&s1, &s2));
    }

    #[tokio::test]
    async fn stale_completion_does_not_free_current_owner() {
        let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
        let current_id = TaskId::next();
        let stale_id = TaskId::next();
        let slot_arc = ctrl.get_or_create_slot("s");
        {
            let mut slot = slot_arc.lock().await;
            slot.status = SlotStatus::Running {
                started_at: Instant::now(),
            };
            slot.running_id = Some(current_id);
        }
        ctrl.running.insert(current_id, Arc::from("s"));

        let mut admissions = JoinSet::new();
        ctrl.handle_completion_result(
            CompletionResult {
                id: stale_id,
                slot_name: Arc::from("s"),
            },
            &mut admissions,
        )
        .await;

        let slot = slot_arc.lock().await;
        assert_eq!(slot.running_id, Some(current_id));
        assert!(matches!(slot.status, SlotStatus::Running { .. }));
        assert!(ctrl.running.contains_key(&current_id));
    }

    #[tokio::test]
    async fn shutdown_finalizes_buffered_submission_as_rejected() {
        let bus = Bus::new(64);
        let ctrl = make_controller(ControllerConfig::default(), bus);

        let task: TaskRef = TaskFn::arc("buffered", |_ctx: TaskContext| async { Ok(()) });
        let (_id, waiter) = ctrl
            .handle()
            .submit_and_watch(ControllerSpec::queue(TaskSpec::once(task)).with_slot("s"))
            .await
            .expect("submission accepted into channel");

        let mut rx = ctrl.rx.write().await.take().expect("rx present");
        ctrl.finalize_pending_on_shutdown(&mut rx);
        drop(rx);

        let outcome = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter must resolve, not hang")
            .expect("waiter must resolve to an outcome, not a dropped sender");
        assert!(
            matches!(outcome, TaskOutcome::Rejected { .. }),
            "a buffered submission on shutdown must resolve Rejected, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn shutdown_rejects_slot_queue_and_clears_controller_state() {
        let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
        let watched_id = TaskId::next();
        let unwatched_id = TaskId::next();
        let running_id = TaskId::next();
        let (done, outcome) = oneshot::channel();
        ctrl.watchers.insert(watched_id, done);

        let slot = ctrl.get_or_create_slot("shutdown-slot");
        {
            let mut slot = slot.lock().await;
            slot.status = SlotStatus::Running {
                started_at: Instant::now(),
            };
            slot.running_id = Some(running_id);
            slot.queue
                .push_back((watched_id, waiting_spec("watched-shutdown-queue")));
            slot.queue
                .push_back((unwatched_id, waiting_spec("plain-shutdown-queue")));
        }
        ctrl.running.insert(running_id, Arc::from("shutdown-slot"));

        ctrl.finalize_slot_state_on_shutdown().await;

        assert!(matches!(
            outcome.await,
            Ok(TaskOutcome::Rejected { reason })
                if reason.as_ref() == crate::reasons::CONTROLLER_SHUTTING_DOWN
        ));
        assert!(ctrl.watchers.is_empty());
        assert!(ctrl.slots.is_empty());
        assert!(ctrl.running.is_empty());
    }

    #[tokio::test]
    async fn shutdown_resolves_buffered_removal_reply() {
        let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
        let (reply, reply_rx) = oneshot::channel();
        ctrl.tx
            .try_send(ControllerCommand::ManageIdentity {
                id: TaskId::next(),
                operation: IdentityOperation::Cancel,
                reply,
            })
            .expect("the controller command channel has capacity");

        let mut rx = ctrl.rx.write().await.take().expect("rx present");
        ctrl.finalize_pending_on_shutdown(&mut rx);

        assert!(matches!(
            reply_rx.await,
            Ok(Err(RuntimeError::ShuttingDown))
        ));
    }

    #[tokio::test]
    async fn aborted_identity_worker_sends_explicit_shutdown_reply() {
        let (reply, reply_rx) = oneshot::channel();
        let (started, started_rx) = oneshot::channel();
        let mut workers = JoinSet::new();
        workers.spawn(async move {
            let _reply = IdentityReply::new(reply);
            let _ = started.send(());
            std::future::pending::<()>().await;
        });
        tokio::time::timeout(Duration::from_secs(1), started_rx)
            .await
            .expect("the identity worker must start")
            .expect("the identity worker must signal start");

        workers.abort_all();
        Controller::drain_workers(&mut workers).await;

        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), reply_rx).await,
            Ok(Ok(Err(RuntimeError::ShuttingDown)))
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn controller_task_join_can_resume_after_a_dropped_waiter() {
        let (release, released) = oneshot::channel::<()>();
        let task = Arc::new(ControllerTask::new(tokio::spawn(async move {
            let _ = released.await;
        })));
        let bus = Bus::new(8);

        let first_task = Arc::clone(&task);
        let first_bus = bus.clone();
        let first = tokio::spawn(async move { first_task.join(&first_bus).await });
        assert!(
            poll_until(Duration::from_secs(1), || async {
                task.state.try_lock().is_err()
            })
            .await,
            "the first waiter must own the shared join state"
        );
        first.abort();
        let _ = first.await;
        assert!(
            poll_until(Duration::from_secs(1), || async {
                task.state.try_lock().is_ok()
            })
            .await,
            "aborting the first waiter must release the shared join state"
        );

        let second_task = Arc::clone(&task);
        let second_bus = bus.clone();
        let second = tokio::spawn(async move { second_task.join(&second_bus).await });
        assert!(
            poll_until(Duration::from_secs(1), || async {
                task.state.try_lock().is_err()
            })
            .await,
            "the second waiter must resume ownership of the stored JoinHandle"
        );
        assert!(
            !second.is_finished(),
            "the stored JoinHandle must remain pending after the first waiter is dropped"
        );

        release.send(()).expect("the controller task is waiting");
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), second).await,
            Ok(Ok(true))
        ));
        assert!(task.is_joined().await);
    }

    #[tokio::test]
    async fn submit_after_shutdown_finalize_is_rejected_not_leaked() {
        let bus = Bus::new(64);
        let ctrl = make_controller(ControllerConfig::default(), bus);

        let mut rx = ctrl.rx.write().await.take().expect("rx present");
        ctrl.finalize_pending_on_shutdown(&mut rx);

        let task: TaskRef = TaskFn::arc("late", |_ctx: TaskContext| async { Ok(()) });
        let result = ctrl
            .handle()
            .submit_and_watch(ControllerSpec::queue(TaskSpec::once(task)).with_slot("s"))
            .await;

        assert!(
            result.is_err(),
            "a submission after shutdown finalization must be rejected, not handed a doomed waiter"
        );
        drop(rx);
    }

    fn make_controller(config: ControllerConfig, bus: Bus) -> Controller {
        let (tx, rx) = mpsc::channel(config.queue_capacity().get());
        Controller {
            config,
            supervisor: Weak::new(),
            bus,
            shutdown_token: CancellationToken::new(),
            slots: DashMap::new(),
            running: DashMap::new(),
            watchers: DashMap::new(),
            tx,
            rx: RwLock::new(Some(rx)),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            task: OnceLock::new(),
        }
    }

    #[tokio::test]
    async fn guarded_converts_panic_to_diagnostic_and_survives() {
        let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
        let mut rx = ctrl.bus.subscribe();

        let _ = ctrl.guarded("unit", async { panic!("boom {}", 1) }).await;

        let ev = rx
            .try_recv()
            .expect("a panicking work-unit must publish a diagnostic");
        assert_eq!(ev.kind, EventKind::ControllerRejected);
        assert!(
            ev.reason.as_deref().unwrap_or_default().contains("boom 1"),
            "diagnostic must carry the panic message, got {:?}",
            ev.reason
        );
    }

    #[tokio::test]
    async fn minimum_queue_capacity_is_supported() {
        let sup = Supervisor::builder(crate::SupervisorConfig::default())
            .with_controller(
                ControllerConfig::default()
                    .with_queue_capacity(NonZeroUsize::new(1).unwrap())
                    .with_max_slot_queue(1),
            )
            .build();
        let handle = sup.serve();

        let task: TaskRef = TaskFn::arc("minimum-capacity", |_ctx: TaskContext| async { Ok(()) });
        handle
            .submit(ControllerSpec::queue(TaskSpec::once(task)))
            .await
            .expect("submission must work with the minimum non-zero capacity");

        let _ = handle.shutdown().await;
    }

    fn waiting_spec(name: &'static str) -> TaskSpec {
        let task: TaskRef = TaskFn::arc(name, |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        TaskSpec::restartable(task)
    }

    async fn start_controller_loop(
        ctrl: &Arc<Controller>,
        token: &CancellationToken,
    ) -> tokio::task::JoinHandle<Result<(), ControllerError>> {
        let runner_ctrl = Arc::clone(ctrl);
        let runner_token = token.clone();
        let runner = tokio::spawn(async move { runner_ctrl.run_inner(runner_token).await });

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if ctrl.rx.read().await.is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("controller loop must take its command receiver");
        runner
    }

    async fn stop_controller_loop(
        token: CancellationToken,
        runner: tokio::task::JoinHandle<Result<(), ControllerError>>,
    ) {
        token.cancel();
        tokio::time::timeout(Duration::from_secs(1), runner)
            .await
            .expect("controller loop must stop after cancellation")
            .expect("controller loop task must not panic")
            .expect("controller loop must exit cleanly");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn public_shutdown_waits_for_controller_join_and_survives_a_dropped_waiter() {
        let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
        let _runtime_handle = sup.serve();
        let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));
        sup.core().attach_controller(&ctrl);
        ctrl.run();
        ctrl.run();

        let handle = crate::core::SupervisorHandle::new(Arc::clone(sup.owner()))
            .with_controller(Some(Arc::clone(&ctrl)));
        let slot = ctrl.get_or_create_slot("blocked-shutdown-slot");
        let slot_guard = slot.lock().await;

        handle
            .submit(
                ControllerSpec::queue(waiting_spec("blocked-shutdown-task"))
                    .with_slot("blocked-shutdown-slot"),
            )
            .await
            .expect("the blocking submission must enter the controller queue");
        assert!(
            poll_until(Duration::from_secs(2), || async {
                ctrl.tx.capacity() == ctrl.config.queue_capacity().get()
            })
            .await,
            "the controller must receive the command and block on the held slot lock"
        );

        let (_queued_id, queued_waiter) = handle
            .submit_and_watch(
                ControllerSpec::queue(waiting_spec("buffered-during-shutdown"))
                    .with_slot("buffered-during-shutdown"),
            )
            .await
            .expect("the watched command must be buffered behind the blocked handler");
        let (_panicking_id, panicking_waiter) = handle
            .submit_and_watch(ControllerSpec::queue(TaskSpec::once(Arc::new(
                PanickingNameTask,
            ))))
            .await
            .expect("the hostile watched command must remain buffered for shutdown drain");
        let identity_handle = handle.clone();
        let identity = tokio::spawn(async move { identity_handle.cancel(TaskId::next()).await });
        assert!(
            poll_until(Duration::from_secs(2), || async {
                ctrl.tx.capacity() == ctrl.config.queue_capacity().get() - 3
            })
            .await,
            "all later commands must remain buffered before shutdown"
        );

        let first_handle = handle.clone();
        let first_shutdown = tokio::spawn(async move { first_handle.shutdown().await });
        assert!(
            poll_until(Duration::from_secs(2), || async {
                sup.core().is_shutting_down()
            })
            .await,
            "shared runtime shutdown must start"
        );
        assert!(
            poll_until(Duration::from_secs(2), || async {
                ctrl.task
                    .get()
                    .is_some_and(|task| task.state.try_lock().is_err())
            })
            .await,
            "the shared shutdown owner must reach the controller join"
        );
        assert!(
            !first_shutdown.is_finished(),
            "public shutdown must wait for the blocked controller loop"
        );

        first_shutdown.abort();
        let _ = first_shutdown.await;

        let second_shutdown = tokio::spawn(async move { handle.shutdown().await });
        tokio::task::yield_now().await;
        assert!(
            !second_shutdown.is_finished(),
            "dropping one shutdown waiter must not detach the shared controller join"
        );

        drop(slot_guard);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), second_shutdown).await,
            Ok(Ok(Ok(())))
        ));
        assert!(ctrl.is_joined().await);
        assert!(ctrl.slots.is_empty());
        assert!(ctrl.running.is_empty());
        assert!(ctrl.watchers.is_empty());
        let queued_outcome = tokio::time::timeout(Duration::from_millis(50), queued_waiter.wait())
            .await
            .expect("the buffered watcher must already be settled")
            .expect("the buffered watched command must resolve before shutdown returns");
        assert!(matches!(
            queued_outcome,
            TaskOutcome::Rejected { reason }
                if reason.as_ref() == crate::reasons::CONTROLLER_SHUTTING_DOWN
        ));
        let panicking_outcome =
            tokio::time::timeout(Duration::from_millis(50), panicking_waiter.wait())
                .await
                .expect("the hostile buffered watcher must already be settled")
                .expect("the hostile buffered watcher must resolve as an outcome");
        assert!(matches!(
            panicking_outcome,
            TaskOutcome::Rejected { reason }
                if reason.as_ref() == crate::reasons::CONTROLLER_SHUTTING_DOWN
        ));
        assert!(identity.is_finished());
        assert!(matches!(
            identity.await,
            Ok(Err(RuntimeError::ShuttingDown))
        ));

        let late = ctrl
            .handle()
            .try_submit(ControllerSpec::queue(waiting_spec("late-after-join")));
        assert!(matches!(late, Err(ControllerError::Closed)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn natural_run_waits_for_controller_join() {
        let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
        let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));
        sup.core().attach_controller(&ctrl);
        ctrl.run();

        let slot = ctrl.get_or_create_slot("blocked-natural-slot");
        let slot_guard = slot.lock().await;
        ctrl.handle()
            .submit(
                ControllerSpec::queue(waiting_spec("blocked-natural-task"))
                    .with_slot("blocked-natural-slot"),
            )
            .await
            .expect("the blocking submission must enter controller intake");
        assert!(
            poll_until(Duration::from_secs(2), || async {
                ctrl.tx.capacity() == ctrl.config.queue_capacity().get()
            })
            .await,
            "the controller must block on the held slot before natural shutdown"
        );

        let run_sup = Arc::clone(&sup);
        let run = tokio::spawn(async move { run_sup.run(vec![]).await });
        assert!(
            poll_until(Duration::from_secs(2), || async {
                ctrl.task
                    .get()
                    .is_some_and(|task| task.state.try_lock().is_err())
            })
            .await,
            "natural shutdown must reach the shared controller join"
        );
        assert!(
            !run.is_finished(),
            "natural run must not return while the controller loop is blocked"
        );

        drop(slot_guard);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), run).await,
            Ok(Ok(Ok(())))
        ));
        assert!(ctrl.is_joined().await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn accepted_cancel_continues_after_caller_future_is_dropped() {
        let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
        let runtime_handle = sup.serve();
        let id = runtime_handle
            .add(waiting_spec("dropped-cancel-caller"))
            .await
            .expect("the direct task must register");

        let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));
        let handle = crate::core::SupervisorHandle::new(Arc::clone(sup.owner()))
            .with_controller(Some(Arc::clone(&ctrl)));

        let mut cancel = Box::pin(handle.cancel(id));
        std::future::poll_fn(|cx| match cancel.as_mut().poll(cx) {
            std::task::Poll::Pending => std::task::Poll::Ready(()),
            std::task::Poll::Ready(result) => {
                panic!("cancel must wait for the stopped controller loop, got {result:?}")
            }
        })
        .await;
        drop(cancel);

        assert_eq!(
            ctrl.tx.capacity(),
            ControllerConfig::default().queue_capacity().get() - 1,
            "the cancel command must be accepted before its caller is dropped"
        );

        let token = CancellationToken::new();
        let runner = start_controller_loop(&ctrl, &token).await;
        assert!(
            poll_until(Duration::from_secs(2), || async {
                handle
                    .list()
                    .await
                    .iter()
                    .all(|(task_id, _)| *task_id != id)
            })
            .await,
            "the controller must complete registry fallback without the public caller"
        );

        stop_controller_loop(token, runner).await;
        let _ = runtime_handle.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn try_remove_reports_full_controller_command_queue() {
        let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
        let runtime_handle = sup.serve();
        let ctrl = Controller::new(
            ControllerConfig::default().with_queue_capacity(NonZeroUsize::new(1).unwrap()),
            sup.core(),
            Bus::new(64),
        );
        let handle = crate::core::SupervisorHandle::new(Arc::clone(sup.owner()))
            .with_controller(Some(Arc::clone(&ctrl)));

        ctrl.handle()
            .try_submit(
                ControllerSpec::queue(waiting_spec("controller-queue-filler")).with_slot("s"),
            )
            .expect("the filler must occupy the controller command queue");

        assert!(matches!(
            handle.try_remove(TaskId::next()).await,
            Err(RuntimeError::CommandQueueFull)
        ));

        let mut rx = ctrl.rx.write().await.take().expect("rx present");
        ctrl.finalize_pending_on_shutdown(&mut rx);
        drop(rx);
        let _ = runtime_handle.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn try_remove_propagates_full_registry_queue_after_controller_admission() {
        let sup = Supervisor::new(
            crate::SupervisorConfig::default()
                .with_registry_queue_capacity(NonZeroUsize::new(1).unwrap()),
            vec![],
        );
        let filler_id = TaskId::next();
        let (_filler_reply, _filler_completion) = sup
            .core()
            .add_task_with_id_watched(filler_id, waiting_spec("registry-queue-filler"), None)
            .expect("the filler must occupy the registry queue");
        assert_eq!(sup.core().registry_command_capacity(), 0);

        let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));
        let handle = crate::core::SupervisorHandle::new(Arc::clone(sup.owner()))
            .with_controller(Some(Arc::clone(&ctrl)));
        let token = CancellationToken::new();
        let runner = start_controller_loop(&ctrl, &token).await;

        assert!(matches!(
            handle.try_remove(TaskId::next()).await,
            Err(RuntimeError::CommandQueueFull)
        ));
        assert_eq!(
            sup.core().registry_command_capacity(),
            0,
            "a rejected fallback must not consume or replace the queued registry command"
        );

        stop_controller_loop(token, runner).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn identity_operation_limit_preserves_command_backpressure() {
        let sup = Supervisor::new(
            crate::SupervisorConfig::default().with_grace(Duration::from_secs(2)),
            vec![],
        );
        let runtime_handle = sup.serve();

        let task_started = Arc::new(AtomicBool::new(false));
        let started = Arc::clone(&task_started);
        let cancellation_observed = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&cancellation_observed);
        let (release, released) = oneshot::channel();
        let released = Arc::new(StdMutex::new(Some(released)));
        let task_release = Arc::clone(&released);
        let task: TaskRef = TaskFn::arc("bounded-identity-owner", move |ctx: TaskContext| {
            let started = Arc::clone(&started);
            let observed = Arc::clone(&observed);
            let released = task_release
                .lock()
                .expect("release lock poisoned")
                .take()
                .expect("the task runs once");
            async move {
                started.store(true, Ordering::SeqCst);
                ctx.cancelled().await;
                observed.store(true, Ordering::SeqCst);
                let _ = released.await;
                Ok(())
            }
        });
        let owner_id = runtime_handle
            .add(TaskSpec::once(task))
            .await
            .expect("the direct task must register");
        assert!(
            poll_until(Duration::from_secs(2), || async {
                task_started.load(Ordering::SeqCst)
            })
            .await,
            "the direct task body must start before cancellation"
        );

        let ctrl = Controller::new(
            ControllerConfig::default().with_queue_capacity(NonZeroUsize::new(1).unwrap()),
            sup.core(),
            Bus::new(64),
        );
        let handle = crate::core::SupervisorHandle::new(Arc::clone(sup.owner()))
            .with_controller(Some(Arc::clone(&ctrl)));
        let token = CancellationToken::new();
        let runner = start_controller_loop(&ctrl, &token).await;

        let cancel_handle = handle.clone();
        let cancel = tokio::spawn(async move {
            cancel_handle
                .cancel_with_timeout(owner_id, Duration::from_secs(10))
                .await
        });
        assert!(
            poll_until(Duration::from_secs(2), || async {
                cancellation_observed.load(Ordering::SeqCst)
            })
            .await,
            "the first identity operation must remain in flight"
        );

        let buffered_ran = Arc::new(AtomicBool::new(false));
        let ran = Arc::clone(&buffered_ran);
        let buffered: TaskRef = TaskFn::arc("buffered-after-identity", move |_ctx| {
            let ran = Arc::clone(&ran);
            async move {
                ran.store(true, Ordering::SeqCst);
                Ok(())
            }
        });
        handle
            .submit(ControllerSpec::queue(TaskSpec::once(buffered)).with_slot("buffered"))
            .await
            .expect("one later command must fit in the bounded controller queue");

        assert!(matches!(
            handle.try_submit(
                ControllerSpec::queue(waiting_spec("overflow-after-identity"))
                    .with_slot("overflow"),
            ),
            Err(ControllerError::Full)
        ));
        assert!(
            !buffered_ran.load(Ordering::SeqCst),
            "the controller must not drain commands past its in-flight identity limit"
        );

        release.send(()).expect("the task is waiting for release");
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), cancel).await,
            Ok(Ok(Ok(true)))
        ));
        assert!(
            poll_until(Duration::from_secs(2), || async {
                buffered_ran.load(Ordering::SeqCst)
            })
            .await,
            "the buffered command must resume after identity cleanup"
        );

        stop_controller_loop(token, runner).await;
        let _ = runtime_handle.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn registry_reply_marks_slot_running_without_task_added() {
        let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
        let handle = sup.serve();
        let controller_bus = Bus::new(1);
        let ctrl = Controller::new(
            ControllerConfig::default(),
            sup.core(),
            controller_bus.clone(),
        );
        let token = CancellationToken::new();
        let runner = start_controller_loop(&ctrl, &token).await;

        let id = ctrl
            .handle()
            .submit(ControllerSpec::queue(waiting_spec("reply-admitted")).with_slot("s"))
            .await
            .expect("controller intake must accept the submission");
        for _ in 0..16 {
            controller_bus.publish(Event::new(EventKind::TaskStarting).with_task("noise"));
        }

        let reached_running = poll_until(Duration::from_secs(2), || async {
            let Some(slot) = ctrl.slots.get("s").map(|entry| entry.clone()) else {
                return false;
            };
            let slot = slot.lock().await;
            slot.running_id == Some(id) && matches!(slot.status, SlotStatus::Running { .. })
        })
        .await;

        assert!(
            reached_running,
            "the direct registry reply must confirm admission without TaskAdded"
        );
        assert_eq!(
            ctrl.running.get(&id).as_deref().map(AsRef::as_ref),
            Some("s")
        );

        stop_controller_loop(token, runner).await;
        assert!(ctrl.running.is_empty());
        assert!(ctrl.slots.is_empty());

        let _ = handle.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn replace_is_processed_while_registry_reply_is_pending() {
        let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
        // Keep TaskRemoved off the controller bus; the reliable completion must advance Replace.
        let controller_bus = Bus::new(64);
        let ctrl = Controller::new(ControllerConfig::default(), sup.core(), controller_bus);
        let token = CancellationToken::new();
        let runner = start_controller_loop(&ctrl, &token).await;

        // The registry is not started yet, so the first committed Add reply stays pending.
        let (first_id, first_outcome) = ctrl
            .handle()
            .submit_and_watch(ControllerSpec::queue(waiting_spec("pending-owner")).with_slot("s"))
            .await
            .expect("controller intake must accept the first submission");
        assert!(
            poll_until(Duration::from_secs(2), || async {
                let Some(slot) = ctrl.slots.get("s").map(|entry| entry.clone()) else {
                    return false;
                };
                let slot = slot.lock().await;
                slot.running_id == Some(first_id)
                    && matches!(slot.status, SlotStatus::Admitting { .. })
            })
            .await,
            "the first Add must remain in flight until the registry starts"
        );

        let replacement_id = ctrl
            .handle()
            .submit(ControllerSpec::replace(waiting_spec("pending-replacement")).with_slot("s"))
            .await
            .expect("controller intake must accept Replace");
        assert!(
            poll_until(Duration::from_secs(2), || async {
                let Some(slot) = ctrl.slots.get("s").map(|entry| entry.clone()) else {
                    return false;
                };
                let slot = slot.lock().await;
                matches!(slot.status, SlotStatus::Terminating { .. })
                    && slot.queue.front().map(|(id, _)| *id) == Some(replacement_id)
            })
            .await,
            "Replace must be processed without waiting for the first registry reply"
        );

        let handle = sup.serve();
        let outcome = tokio::time::timeout(Duration::from_secs(2), first_outcome)
            .await
            .expect("the accepted owner must be removed")
            .expect("the registry must resolve the owner outcome");
        assert!(matches!(outcome, TaskOutcome::Canceled));

        assert!(
            poll_until(Duration::from_secs(2), || async {
                let Some(slot) = ctrl.slots.get("s").map(|entry| entry.clone()) else {
                    return false;
                };
                let slot = slot.lock().await;
                slot.running_id == Some(replacement_id)
                    && matches!(slot.status, SlotStatus::Running { .. })
            })
            .await,
            "the replacement must start from reliable completion without TaskRemoved"
        );

        stop_controller_loop(token, runner).await;
        let _ = handle.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn replace_stays_responsive_under_registry_backpressure() {
        let sup = Supervisor::new(
            crate::SupervisorConfig::default()
                .with_registry_queue_capacity(NonZeroUsize::new(1).unwrap()),
            vec![],
        );
        let runtime_handle = sup.serve();
        let owner_id = runtime_handle
            .add(waiting_spec("replace-owner"))
            .await
            .expect("the owner must register");

        let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));
        let slot_name: Arc<str> = Arc::from("s");
        ctrl.slots.insert(
            Arc::clone(&slot_name),
            Arc::new(Mutex::new(SlotState {
                status: SlotStatus::Running {
                    started_at: Instant::now(),
                },
                running_id: Some(owner_id),
                queue: std::collections::VecDeque::new(),
            })),
        );
        ctrl.running.insert(owner_id, Arc::clone(&slot_name));

        // On a current-thread runtime the registry cannot consume this command until this test
        // yields, so it occupies the only queue slot while both Replace commands are handled.
        let filler_id = TaskId::next();
        let (filler_reply, _filler_completion) = sup
            .core()
            .add_task_with_id_watched(filler_id, waiting_spec("replace-filler"), None)
            .expect("the filler must occupy the registry queue");
        assert_eq!(sup.core().registry_command_capacity(), 0);

        let first_id = TaskId::next();
        let (first_done, first_outcome) = oneshot::channel();
        let first = Submission {
            id: first_id,
            spec: ControllerSpec::replace(waiting_spec("replace-first")).with_slot("s"),
            done: Some(first_done),
        };
        let mut admissions = JoinSet::new();
        let mut removals = JoinSet::new();
        let mut first = Box::pin(ctrl.handle_submission(first, &mut admissions, &mut removals));
        std::future::poll_fn(|cx| match first.as_mut().poll(cx) {
            std::task::Poll::Ready(()) => std::task::Poll::Ready(()),
            std::task::Poll::Pending => {
                panic!("Replace must not wait inside the controller loop for registry capacity")
            }
        })
        .await;
        drop(first);
        assert_eq!(removals.len(), 1, "one owner removal must be tracked");

        let second_id = TaskId::next();
        let second = Submission {
            id: second_id,
            spec: ControllerSpec::replace(waiting_spec("replace-second")).with_slot("s"),
            done: None,
        };
        let mut second = Box::pin(ctrl.handle_submission(second, &mut admissions, &mut removals));
        std::future::poll_fn(|cx| match second.as_mut().poll(cx) {
            std::task::Poll::Ready(()) => std::task::Poll::Ready(()),
            std::task::Poll::Pending => {
                panic!("a newer Replace must stay responsive while removal is backpressured")
            }
        })
        .await;
        drop(second);

        let slot = ctrl
            .slots
            .get("s")
            .map(|entry| entry.clone())
            .expect("the slot must remain tracked");
        let slot = slot.lock().await;
        assert!(matches!(slot.status, SlotStatus::Terminating { .. }));
        assert_eq!(slot.queue.front().map(|(id, _)| *id), Some(second_id));
        drop(slot);
        assert_eq!(
            removals.len(),
            1,
            "repeated Replace must not enqueue duplicate owner removals"
        );
        assert!(matches!(
            first_outcome.await,
            Ok(TaskOutcome::Rejected { reason })
                if reason.as_ref() == crate::reasons::SUPERSEDED_BY_REPLACE
        ));

        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), filler_reply).await,
            Ok(Ok(Ok(())))
        ));
        let removal = tokio::time::timeout(Duration::from_secs(2), removals.join_next())
            .await
            .expect("the owner removal must resume after registry capacity recovers")
            .expect("one removal waiter must exist")
            .expect("the removal waiter must not panic");
        assert_eq!(removal.id, owner_id);
        assert!(matches!(removal.decision, Ok(true)));

        let _ = runtime_handle.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn queued_cancel_is_ordered_without_runtime_bus_events() {
        let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
        let runtime_handle = sup.serve();
        // Runtime lifecycle and removal events cannot reach this controller.
        let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(1));
        let handle = crate::core::SupervisorHandle::new(Arc::clone(sup.owner()))
            .with_controller(Some(Arc::clone(&ctrl)));
        let token = CancellationToken::new();
        let runner = start_controller_loop(&ctrl, &token).await;

        let owner_id = handle
            .submit(ControllerSpec::queue(waiting_spec("cancel-owner")).with_slot("s"))
            .await
            .expect("the owner submission must enter the controller");
        assert!(
            poll_until(Duration::from_secs(2), || async {
                let Some(slot) = ctrl.slots.get("s").map(|entry| entry.clone()) else {
                    return false;
                };
                let slot = slot.lock().await;
                slot.running_id == Some(owner_id)
                    && matches!(slot.status, SlotStatus::Running { .. })
            })
            .await,
            "the first task must own the slot"
        );

        let victim_ran = Arc::new(AtomicBool::new(false));
        let ran = Arc::clone(&victim_ran);
        let victim: TaskRef = TaskFn::arc("cancel-victim", move |_ctx: TaskContext| {
            let ran = Arc::clone(&ran);
            async move {
                ran.store(true, Ordering::SeqCst);
                Ok(())
            }
        });
        let (victim_id, waiter) = handle
            .submit_and_watch(ControllerSpec::queue(TaskSpec::once(victim)).with_slot("s"))
            .await
            .expect("the queued submission must enter the controller channel");

        assert!(
            handle
                .cancel(victim_id)
                .await
                .expect("ordered queued cancellation must succeed"),
            "the first cancellation caller must claim the queued submission"
        );
        let outcome = waiter.wait().await.expect("the queued waiter must resolve");
        assert!(
            matches!(outcome, TaskOutcome::Rejected { reason } if reason.as_ref() == crate::reasons::REMOVED_FROM_QUEUE)
        );

        let try_ran = Arc::clone(&victim_ran);
        let try_victim: TaskRef = TaskFn::arc("try-remove-victim", move |_ctx: TaskContext| {
            let ran = Arc::clone(&try_ran);
            async move {
                ran.store(true, Ordering::SeqCst);
                Ok(())
            }
        });
        let (try_id, try_waiter) = handle
            .submit_and_watch(ControllerSpec::queue(TaskSpec::once(try_victim)).with_slot("s"))
            .await
            .expect("the second queued submission must enter the controller channel");
        assert!(
            handle
                .try_remove(try_id)
                .await
                .expect("the ordered controller channel has capacity"),
            "try_remove must claim queued controller work"
        );
        let try_outcome = try_waiter
            .wait()
            .await
            .expect("the try_remove waiter must resolve");
        assert!(
            matches!(try_outcome, TaskOutcome::Rejected { reason } if reason.as_ref() == crate::reasons::REMOVED_FROM_QUEUE)
        );

        assert!(
            handle
                .cancel(owner_id)
                .await
                .expect("the admitted owner must be cancelled")
        );
        assert!(
            poll_until(Duration::from_secs(2), || async {
                ctrl.slots.get("s").is_none()
                    && !ctrl.running.contains_key(&owner_id)
                    && !ctrl.running.contains_key(&victim_id)
                    && !ctrl.running.contains_key(&try_id)
            })
            .await,
            "the slot must settle after its owner completes"
        );
        assert!(
            !victim_ran.load(Ordering::SeqCst),
            "a queued submission claimed by cancel must never start"
        );

        stop_controller_loop(token, runner).await;
        let _ = runtime_handle.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reliable_completion_reuses_task_name_without_task_removed() {
        let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
        let handle = sup.serve();
        // Registry lifecycle events cannot reach this controller.
        let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(1));
        let token = CancellationToken::new();
        let runner = start_controller_loop(&ctrl, &token).await;

        let log = Arc::new(StdMutex::new(Vec::new()));
        let (release, released) = oneshot::channel();
        let released = Arc::new(StdMutex::new(Some(released)));
        let first_log = Arc::clone(&log);
        let first_release = Arc::clone(&released);
        let first: TaskRef = TaskFn::arc("same-runtime-name", move |_ctx: TaskContext| {
            let released = first_release
                .lock()
                .expect("release lock poisoned")
                .take()
                .expect("the first task runs once");
            let log = Arc::clone(&first_log);
            async move {
                let _ = released.await;
                log.lock().expect("log lock poisoned").push("first");
                Ok(())
            }
        });
        let second_log = Arc::clone(&log);
        let second: TaskRef = TaskFn::arc("same-runtime-name", move |_ctx: TaskContext| {
            let log = Arc::clone(&second_log);
            async move {
                log.lock().expect("log lock poisoned").push("second");
                Ok(())
            }
        });

        let (first_id, first_outcome) = ctrl
            .handle()
            .submit_and_watch(ControllerSpec::queue(TaskSpec::once(first)).with_slot("s"))
            .await
            .expect("the first submission must enter controller intake");
        assert!(
            poll_until(Duration::from_secs(2), || async {
                let Some(slot) = ctrl.slots.get("s").map(|entry| entry.clone()) else {
                    return false;
                };
                let slot = slot.lock().await;
                slot.running_id == Some(first_id)
                    && matches!(slot.status, SlotStatus::Running { .. })
            })
            .await,
            "the first task must own the slot before queueing the second"
        );

        let (second_id, second_outcome) = ctrl
            .handle()
            .submit_and_watch(ControllerSpec::queue(TaskSpec::once(second)).with_slot("s"))
            .await
            .expect("the second submission must enter controller intake");
        assert!(
            poll_until(Duration::from_secs(2), || async {
                let Some(slot) = ctrl.slots.get("s").map(|entry| entry.clone()) else {
                    return false;
                };
                slot.lock().await.queue.front().map(|(id, _)| *id) == Some(second_id)
            })
            .await,
            "the second task must wait behind the first"
        );

        release.send(()).expect("the first task is waiting");
        let first_outcome = tokio::time::timeout(Duration::from_secs(2), first_outcome)
            .await
            .expect("the first outcome must arrive")
            .expect("the registry must send the first outcome");
        let second_outcome = tokio::time::timeout(Duration::from_secs(2), second_outcome)
            .await
            .expect("reliable completion must start the queued task")
            .expect("the registry must send the second outcome");
        assert!(matches!(first_outcome, TaskOutcome::Completed));
        assert!(matches!(second_outcome, TaskOutcome::Completed));
        assert_eq!(
            log.lock().expect("log lock poisoned").as_slice(),
            ["first", "second"]
        );
        assert!(
            poll_until(Duration::from_secs(2), || async {
                ctrl.slots.get("s").is_none()
                    && !ctrl.running.contains_key(&first_id)
                    && !ctrl.running.contains_key(&second_id)
            })
            .await,
            "the empty slot must be collected after the second completion"
        );

        stop_controller_loop(token, runner).await;
        let _ = handle.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn duplicate_reply_frees_slot_without_task_add_failed() {
        let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
        let handle = sup.serve();
        handle
            .add(waiting_spec("duplicate-reply"))
            .await
            .expect("the existing task must register");

        // The controller listens to a different bus, so TaskAddFailed cannot drive its state.
        let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(1));
        let token = CancellationToken::new();
        let runner = start_controller_loop(&ctrl, &token).await;
        let (id, outcome) = ctrl
            .handle()
            .submit_and_watch(ControllerSpec::queue(waiting_spec("duplicate-reply")).with_slot("s"))
            .await
            .expect("controller intake must accept the duplicate");

        let outcome = tokio::time::timeout(Duration::from_secs(2), outcome)
            .await
            .expect("registry rejection must resolve the watcher")
            .expect("registry must send a rejected outcome");
        assert!(
            matches!(outcome, TaskOutcome::Rejected { reason } if reason.as_ref() == crate::reasons::ALREADY_EXISTS)
        );
        assert!(
            poll_until(Duration::from_secs(2), || async {
                !ctrl.running.contains_key(&id) && !ctrl.watchers.contains_key(&id)
            })
            .await,
            "the rejected admission must release its slot ownership"
        );
        assert!(
            ctrl.slots.get("s").is_none(),
            "an idle empty slot should be collected after registry rejection"
        );

        stop_controller_loop(token, runner).await;
        let _ = handle.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn queued_admission_skips_registry_rejected_head() {
        let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
        let handle = sup.serve();
        handle
            .add(waiting_spec("queued-duplicate"))
            .await
            .expect("the existing task must register");
        let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(1));
        let slot_name: Arc<str> = Arc::from("s");
        let slot_arc = ctrl.get_or_create_slot(&slot_name);
        let duplicate_id = TaskId::next();
        let accepted_id = TaskId::next();
        let (duplicate_done, duplicate_outcome) = oneshot::channel();
        let (accepted_done, _accepted_outcome) = oneshot::channel();
        ctrl.watchers.insert(duplicate_id, duplicate_done);
        ctrl.watchers.insert(accepted_id, accepted_done);

        let mut admissions = JoinSet::new();
        let mut completions = JoinSet::new();
        let mut removals = JoinSet::new();
        {
            let mut slot = slot_arc.lock().await;
            slot.queue
                .push_back((duplicate_id, waiting_spec("queued-duplicate")));
            slot.queue
                .push_back((accepted_id, waiting_spec("queued-accepted")));
            ctrl.start_next_from_queue(sup.core(), &mut slot, &slot_name, &mut admissions);
        }

        for _ in 0..2 {
            let result = tokio::time::timeout(Duration::from_secs(2), admissions.join_next())
                .await
                .expect("registry admission reply must arrive")
                .expect("one admission must be in flight")
                .expect("admission waiter must not fail");
            ctrl.handle_admission_result(result, &mut admissions, &mut completions, &mut removals)
                .await;
        }

        let duplicate_outcome = duplicate_outcome
            .await
            .expect("registry must resolve the duplicate watcher");
        assert!(
            matches!(duplicate_outcome, TaskOutcome::Rejected { reason } if reason.as_ref() == crate::reasons::ALREADY_EXISTS)
        );
        let slot = slot_arc.lock().await;
        assert_eq!(slot.running_id, Some(accepted_id));
        assert!(matches!(slot.status, SlotStatus::Running { .. }));
        assert!(slot.queue.is_empty());
        drop(slot);
        assert!(!ctrl.running.contains_key(&duplicate_id));
        assert_eq!(
            ctrl.running.get(&accepted_id).as_deref().map(AsRef::as_ref),
            Some("s")
        );

        let _ = handle.shutdown().await;
    }

    async fn sup_with_live_task() -> (Arc<Supervisor>, crate::core::SupervisorHandle, TaskId) {
        let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
        let handle = sup.serve();
        let task: TaskRef = TaskFn::arc("occupant", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        let id = handle
            .add(TaskSpec::restartable(task))
            .await
            .expect("task should register");
        (sup, handle, id)
    }

    #[tokio::test]
    async fn no_queue_advancement_after_shutdown_starts() {
        let (sup, handle, id) = sup_with_live_task().await;
        let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));

        let queued: TaskRef = TaskFn::arc("queued", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        let mut queue = std::collections::VecDeque::new();
        queue.push_back((TaskId::next(), TaskSpec::restartable(queued)));
        ctrl.slots.insert(
            Arc::from("s"),
            Arc::new(Mutex::new(SlotState {
                status: SlotStatus::Running {
                    started_at: Instant::now(),
                },
                running_id: Some(id),
                queue,
            })),
        );
        ctrl.running.insert(id, Arc::from("s"));
        let mut admissions = JoinSet::new();
        ctrl.mark_shutting_down();
        ctrl.handle_completion_result(
            CompletionResult {
                id,
                slot_name: Arc::from("s"),
            },
            &mut admissions,
        )
        .await;

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            sup.core().id_for_label("queued").await.is_none(),
            "controller must not start queued tasks once shutdown has been requested"
        );

        let _ = handle.shutdown().await;
    }

    #[tokio::test]
    async fn replace_supersedes_in_same_slot() {
        let sup = Supervisor::builder(crate::SupervisorConfig::default())
            .with_controller(ControllerConfig::default())
            .build();
        let handle = sup.serve();

        let mk = |name: &'static str| -> ControllerSpec {
            let task: TaskRef = TaskFn::arc(name, |ctx: TaskContext| async move {
                ctx.cancelled().await;
                Ok(())
            });
            ControllerSpec::replace(TaskSpec::restartable(task)).with_slot("s")
        };

        handle.submit(mk("run-1")).await.unwrap();
        handle.submit(mk("run-2")).await.unwrap();

        let superseded = poll_until(std::time::Duration::from_secs(3), || async {
            let alive = handle.snapshot().await;
            alive.iter().any(|n| &**n == "run-2") && alive.iter().all(|n| &**n != "run-1")
        })
        .await;
        assert!(
            superseded,
            "Replace must supersede run-1 with run-2 in the shared slot, not run both"
        );

        let _ = handle.shutdown().await;
    }

    #[tokio::test]
    async fn snapshot_reports_status_running_and_queue_depth() {
        use crate::controller::SlotStatusKind;

        let (sup, handle, id) = sup_with_live_task().await;
        let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));

        let queued: TaskRef = TaskFn::arc("queued", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        let mut queue = std::collections::VecDeque::new();
        queue.push_back((TaskId::next(), TaskSpec::restartable(queued)));
        ctrl.slots.insert(
            Arc::from("s"),
            Arc::new(Mutex::new(SlotState {
                status: SlotStatus::Running {
                    started_at: Instant::now(),
                },
                running_id: Some(id),
                queue,
            })),
        );

        let snap = ctrl.snapshot().await;
        assert_eq!(snap.len(), 1, "one slot tracked");
        assert_eq!(snap.running_count(), 1);
        assert_eq!(snap.total_queued(), 1);

        let view = snap.slot("s").expect("slot 's' must be present");
        assert_eq!(view.status, SlotStatusKind::Running);
        assert_eq!(view.queue_depth, 1);
        assert_eq!(view.running, Some(id));

        let _ = handle.shutdown().await;
    }

    async fn poll_until<F, Fut>(within: std::time::Duration, mut cond: F) -> bool
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = bool>,
    {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            if cond().await {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
