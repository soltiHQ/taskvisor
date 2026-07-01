//! Internal controller engine.
//!
//! The controller is the slot-based admission layer behind `SupervisorHandle::submit`, `try_submit`, and `submit_and_watch`.
//!
//! It owns:
//!
//! - the bounded intake channel for incoming [`ControllerSpec`] submissions,
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
//! Admitting -- TaskAdded --> Running -- TaskRemoved --> Idle or next queued submission
//!   |                         |
//!   | TaskAddFailed           | Replace
//!   v                         v
//! Idle or next queued       Terminating -- TaskRemoved --> Idle or next queued submission
//! ```
//!
//! The controller advances slots from runtime events.
//! It starts queued work only after `TaskRemoved`; the registry has finished removing the previous task before the next one is added.
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
    sync::{Arc, Weak},
    time::Instant,
};

use dashmap::DashMap;
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use tokio::sync::broadcast;

use crate::{
    RuntimeError, TaskSpec,
    core::{OutcomeTx, SupervisorCore, TaskOutcome},
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
mod recovery;
mod shutdown;

/// Submission accepted by the controller intake channel.
struct Submission {
    /// Pre-minted identity used for events, slot state, and final outcome correlation.
    id: TaskId,
    /// Admission policy, task spec, and optional slot key.
    spec: ControllerSpec,
    /// Optional watched-outcome sender for `submit_and_watch`.
    done: Option<OutcomeTx>,
}

/// Internal handle used by `SupervisorHandle` to submit work to the controller.
#[derive(Clone)]
pub(crate) struct ControllerHandle {
    tx: mpsc::Sender<Submission>,
}

impl ControllerHandle {
    /// Sends a submission to the controller intake channel.
    ///
    /// This waits for intake-channel capacity.
    /// `Ok(id)` means the controller received the submission.
    /// It does not mean the task has been admitted to the runtime yet.
    pub async fn submit(&self, spec: ControllerSpec) -> Result<TaskId, ControllerError> {
        let id = TaskId::next();
        self.tx
            .send(Submission {
                id,
                spec,
                done: None,
            })
            .await
            .map_err(|_| ControllerError::Closed)?;
        Ok(id)
    }

    /// Tries to send a submission without waiting for intake-channel capacity.
    ///
    /// `ControllerError::Full` means the controller intake channel is full.
    /// It does not mean the target slot queue is full.
    pub fn try_submit(&self, spec: ControllerSpec) -> Result<TaskId, ControllerError> {
        let id = TaskId::next();
        self.tx
            .try_send(Submission {
                id,
                spec,
                done: None,
            })
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => ControllerError::Full,
                mpsc::error::TrySendError::Closed(_) => ControllerError::Closed,
            })?;
        Ok(id)
    }

    /// Sends a watched submission to the controller intake channel.
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
            .send(Submission {
                id,
                spec,
                done: Some(tx),
            })
            .await
            .map_err(|_| ControllerError::Closed)?;
        Ok((id, rx))
    }
}

/// Slot-based admission controller.
///
/// The controller is driven by two inputs:
/// - submissions from its intake channel,
/// - runtime lifecycle events from the bus.
///
/// It is intentionally event-driven.
/// Slot advancement happens on `TaskAdded`, `TaskAddFailed`, `TaskRemoveRequested`, `TaskRemoved`, and `ShutdownRequested`.
pub(crate) struct Controller {
    /// Static controller configuration.
    config: ControllerConfig,
    /// Runtime control surface. Weak avoids an ownership cycle with the supervisor.
    supervisor: Weak<SupervisorCore>,
    /// Runtime event bus used for lifecycle input and controller diagnostics.
    bus: Bus,
    /// Per-slot mutable state.
    slots: DashMap<Arc<str>, Arc<Mutex<SlotState>>>,
    /// Reverse index from current runtime task id to slot name.
    running: DashMap<TaskId, Arc<str>>,
    /// Watched submissions not yet handed to the runtime registry.
    watchers: DashMap<TaskId, OutcomeTx>,
    /// Intake sender cloned into `ControllerHandle`.
    tx: mpsc::Sender<Submission>,
    /// Single-use intake receiver owned by the controller loop.
    rx: RwLock<Option<mpsc::Receiver<Submission>>>,
    /// Set after `ShutdownRequested` is observed.
    shutting_down: std::sync::atomic::AtomicBool,
}

impl Controller {
    /// Creates a controller and its bounded intake channel.
    ///
    /// The controller is inert until [`run`](Self::run) is called.
    /// `queue_capacity = 0` is clamped to `1`.
    pub fn new(config: ControllerConfig, supervisor: &Arc<SupervisorCore>, bus: Bus) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(config.queue_capacity.max(1));

        Arc::new(Self {
            config,
            supervisor: Arc::downgrade(supervisor),
            bus,
            slots: DashMap::new(),
            running: DashMap::new(),
            watchers: DashMap::new(),
            tx,
            rx: RwLock::new(Some(rx)),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
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

    /// Returns `true` after the controller has observed `ShutdownRequested`.
    fn is_shutting_down(&self) -> bool {
        self.shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Returns a cloneable handle for sending controller submissions.
    pub fn handle(&self) -> ControllerHandle {
        ControllerHandle {
            tx: self.tx.clone(),
        }
    }

    /// Spawns the controller loop.
    ///
    /// The intake receiver is single-use.
    /// If the loop is started twice, the spawned task publishes a controller diagnostic event.
    pub fn run(self: Arc<Self>, token: CancellationToken) {
        let bus = self.bus.clone();
        tokio::spawn(async move {
            if let Err(e) = self.run_inner(token).await {
                bus.publish(
                    Event::new(EventKind::ControllerRejected)
                        .with_task("controller")
                        .with_reason(format!("controller_loop_exited: {e}")),
                );
            }
        });
    }

    /// Runs the controller event loop.
    ///
    /// The loop receives submissions from the intake channel and lifecycle events from the runtime bus.
    ///
    /// On shutdown, it closes the intake receiver, drains buffered submissions, and resolves pending watched submissions as `Rejected`.
    async fn run_inner(&self, token: CancellationToken) -> Result<(), ControllerError> {
        let mut rx = self
            .rx
            .write()
            .await
            .take()
            .ok_or(ControllerError::AlreadyRunning)?;

        let mut bus_rx = self.bus.subscribe();
        loop {
            tokio::select! {
                _ = token.cancelled() => break,

                Some(sub) = rx.recv() => {
                    self.guarded("handle_submission", self.handle_submission(sub)).await;
                }
                result = bus_rx.recv() => {
                    match result {
                        Ok(event) => {
                            self.guarded("handle_event", self.handle_event(event)).await;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            self.bus.publish(
                                Event::new(EventKind::ControllerRejected)
                                    .with_task("controller")
                                    .with_reason(format!("bus_lagged: missed {n} events, recovering slots")),
                            );
                            self.guarded("recover_stale_slots", self.recover_stale_slots()).await;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
        self.finalize_pending_on_shutdown(&mut rx);
        Ok(())
    }

    /// Runs one controller work unit behind a panic boundary.
    ///
    /// A panic is converted into a diagnostic `ControllerRejected` event and the loop continues.
    ///
    /// This guard does not repair partially updated slot state by itself.
    /// Callers that park watcher state must still make sure the watcher is resolved or returned on every failure path.
    async fn guarded(&self, who: &'static str, fut: impl Future<Output = ()>) {
        if let Err(msg) = crate::core::panic_guard::guarded(fut).await {
            self.bus.publish(
                Event::new(EventKind::ControllerRejected)
                    .with_task("controller")
                    .with_reason(format!("{who}_panicked: {msg}")),
            );
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
    /// A slot becomes `Running` only after `TaskAdded`.
    async fn handle_submission(&self, sub: Submission) {
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
                    .with_reason("controller_shutting_down"),
            );
            self.finalize_rejected(id, "controller_shutting_down");
            return;
        }

        let slot_name: Arc<str> = Arc::from(spec.slot_name());
        let admission = spec.admission;
        let task_spec = spec.task_spec;

        let slot_arc = self.get_or_create_slot(&slot_name);
        let mut slot = slot_arc.lock().await;

        match (&slot.status, admission) {
            (SlotStatus::Idle, _) => {
                match self.start_in_slot(&sup, &mut slot, &slot_name, id, task_spec) {
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
                if let Some(rid) = slot.running_id
                    && let Err(e) = sup.remove(rid)
                {
                    let reason = format!("remove_failed: {e}");
                    self.bus.publish(
                        Event::new(EventKind::ControllerRejected)
                            .with_task(Arc::clone(&slot_name))
                            .with_id(id)
                            .with_reason(reason.clone()),
                    );
                    self.finalize_rejected(id, &reason);
                    return;
                }
                self.replace_head_or_push(&mut slot, &slot_name, id, task_spec);
                slot.status = SlotStatus::Terminating {
                    cancelled_at: Instant::now(),
                };
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

    /// Routes runtime bus events that can affect controller slot state.
    ///
    /// Events without a matching current slot owner are ignored as stale or unrelated.
    async fn handle_event(&self, event: Arc<Event>) {
        match event.kind {
            EventKind::TaskAdded => self.on_task_added(&event).await,
            EventKind::TaskRemoved => self.on_task_finished(&event).await,
            EventKind::TaskAddFailed => self.on_task_add_failed(&event).await,
            EventKind::TaskRemoveRequested => self.on_remove_requested(&event).await,
            EventKind::ShutdownRequested => {
                self.shutting_down
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            _ => {}
        }
    }

    /// Handles removal of a queued, not-yet-admitted submission.
    ///
    /// If the removed id is still waiting in a slot queue, the controller removes it and resolves its watcher as `Rejected("removed_from_queue")`.
    ///
    /// Already-admitted tasks are removed by the runtime path, not here.
    async fn on_remove_requested(&self, event: &Event) {
        let Some(id) = event.id else {
            return;
        };
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
            let Some(pos) = slot.queue.iter().position(|(qid, _)| *qid == id) else {
                continue;
            };
            slot.queue.remove(pos);
            self.bus.publish(
                Event::new(EventKind::ControllerRejected)
                    .with_task(Arc::clone(&slot_name))
                    .with_id(id)
                    .with_reason("removed_from_queue"),
            );
            self.finalize_rejected(id, "removed_from_queue");
            self.gc_if_idle(&slot_name, slot);
            return;
        }
    }

    /// Handles `TaskAdded` for the current slot owner.
    ///
    /// Normal path: `Admitting` becomes `Running`.
    ///
    /// If `Replace` arrived while the task was still admitting, the slot is already `Terminating`;
    /// now that the task exists in the runtime, removal is requested.
    async fn on_task_added(&self, event: &Event) {
        let Some(id) = event.id else {
            return;
        };
        let Some(sup) = self.supervisor.upgrade() else {
            return;
        };
        let Some(slot_name) = self.running.get(&id).map(|e| e.clone()) else {
            return;
        };
        let Some(slot_arc) = self.slots.get(&*slot_name).map(|e| e.clone()) else {
            return;
        };
        let mut slot = slot_arc.lock().await;
        if slot.running_id != Some(id) {
            return;
        }
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
                if let Some(rid) = slot.running_id
                    && let Err(e) = sup.remove(rid)
                {
                    self.bus.publish(
                        Event::new(EventKind::ControllerRejected)
                            .with_task(slot_name)
                            .with_reason(format!("remove_failed: {e}")),
                    );
                }
            }
            _ => {}
        }
    }

    /// Handles runtime registration failure for the current slot owner.
    ///
    /// This can happen after controller admission, for example when the runtime registry rejects a duplicate task name.
    async fn on_task_add_failed(&self, event: &Event) {
        self.free_and_advance(event).await;
    }

    /// Handles `TaskRemoved` for the current slot owner.
    ///
    /// Correlation by `TaskId` makes stale or duplicate removals harmless no-ops.
    async fn on_task_finished(&self, event: &Event) {
        self.free_and_advance(event).await;
    }

    /// Frees a slot after `TaskRemoved` or `TaskAddFailed`.
    ///
    /// The event id must match the current slot owner.
    /// If it does, the slot is reset to `Idle` and, unless shutdown is active, the next queued submission is started.
    async fn free_and_advance(&self, event: &Event) {
        let Some(id) = event.id else {
            return;
        };
        let Some(sup) = self.supervisor.upgrade() else {
            return;
        };
        let Some((_, slot_name)) = self.running.remove(&id) else {
            return;
        };
        let Some(slot_arc) = self.slots.get(&*slot_name).map(|e| e.clone()) else {
            return;
        };
        let mut slot = slot_arc.lock().await;
        if slot.running_id != Some(id) {
            return;
        }

        slot.running_id = None;
        slot.status = SlotStatus::Idle;

        if !self.is_shutting_down() {
            self.start_next_from_queue(&sup, &mut slot, &slot_name);
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
    ) -> Result<(), RuntimeError> {
        let done = self.watchers.remove(&id).map(|(_, tx)| tx);
        match sup.add_task_with_id_watched(id, task_spec, done) {
            Ok(_) => {
                slot.status = SlotStatus::Admitting {
                    since: Instant::now(),
                };
                slot.running_id = Some(id);
                self.running.insert(id, Arc::clone(slot_name));
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
    ) {
        while let Some((next_id, next_spec)) = slot.queue.pop_front() {
            match self.start_in_slot(sup, slot, slot_name, next_id, next_spec) {
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
        if slot_len >= self.config.max_slot_queue {
            let reason = format!("queue_full: {}/{}", slot_len, self.config.max_slot_queue);
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
                    .with_reason("superseded_by_replace"),
            );
            self.finalize_rejected(displaced_id, "superseded_by_replace");
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
    use crate::{BackoffPolicy, RestartPolicy, TaskFn, TaskRef, TaskSpec};
    use std::time::Duration;

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
        assert_eq!(ev.reason.as_deref(), Some("superseded_by_replace"));
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
        let config = ControllerConfig {
            queue_capacity: 16,
            max_slot_queue: 3,
        };
        let ctrl = make_controller(config, bus);
        assert!(!ctrl.reject_if_full("slot", TaskId::next(), 0));
        assert!(!ctrl.reject_if_full("slot", TaskId::next(), 2));
    }

    #[test]
    fn reject_if_full_returns_true_at_capacity() {
        let bus = Bus::new(64);
        let config = ControllerConfig {
            queue_capacity: 16,
            max_slot_queue: 3,
        };
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
    async fn handle_event_ignores_non_task_removed() {
        let bus = Bus::new(64);
        let ctrl = make_controller(ControllerConfig::default(), bus);

        let slot_arc = ctrl.get_or_create_slot("t");
        {
            let mut slot = slot_arc.lock().await;
            slot.status = SlotStatus::Running {
                started_at: Instant::now(),
            };
        }

        let event = Arc::new(Event::new(EventKind::TaskFailed).with_task("t"));
        ctrl.handle_event(event).await;

        let slot = slot_arc.lock().await;
        assert!(
            matches!(slot.status, SlotStatus::Running { .. }),
            "non-TaskRemoved events should not affect slot state"
        );
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
        let (tx, rx) = mpsc::channel(config.queue_capacity.max(1));
        Controller {
            config,
            supervisor: Weak::new(),
            bus,
            slots: DashMap::new(),
            running: DashMap::new(),
            watchers: DashMap::new(),
            tx,
            rx: RwLock::new(Some(rx)),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
        }
    }

    #[tokio::test]
    async fn guarded_converts_panic_to_diagnostic_and_survives() {
        let ctrl = make_controller(ControllerConfig::default(), Bus::new(64));
        let mut rx = ctrl.bus.subscribe();

        ctrl.guarded("unit", async { panic!("boom {}", 1) }).await;

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
    async fn zero_queue_capacity_is_clamped_not_panicking() {
        let sup = Supervisor::builder(crate::SupervisorConfig::default())
            .with_controller(ControllerConfig {
                queue_capacity: 0,
                max_slot_queue: 1,
            })
            .build();
        let handle = sup.serve();

        let task: TaskRef = TaskFn::arc("clamped", |_ctx: TaskContext| async { Ok(()) });
        handle
            .submit(ControllerSpec::queue(TaskSpec::once(task)))
            .await
            .expect("submission must work with capacity clamped to 1");

        let _ = handle.shutdown().await;
    }

    fn long_ago() -> Instant {
        Instant::now()
            .checked_sub(Duration::from_secs(60))
            .expect("test host uptime must exceed one minute")
    }

    async fn sup_with_live_task() -> (Arc<Supervisor>, crate::core::SupervisorHandle, TaskId) {
        let sup = Supervisor::new(crate::SupervisorConfig::default(), vec![]);
        let handle = sup.serve();
        let task: TaskRef = TaskFn::arc("occupant", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        let id = handle
            .add_and_wait(TaskSpec::restartable(task), Duration::from_secs(1))
            .await
            .expect("task should register");
        (sup, handle, id)
    }

    #[tokio::test]
    async fn recover_promotes_admitting_slot_with_alive_task() {
        let (sup, handle, id) = sup_with_live_task().await;
        let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));

        ctrl.slots.insert(
            Arc::from("s"),
            Arc::new(Mutex::new(SlotState {
                status: SlotStatus::Admitting { since: long_ago() },
                running_id: Some(id),
                queue: std::collections::VecDeque::new(),
            })),
        );
        ctrl.running.insert(id, Arc::from("s"));

        ctrl.recover_stale_slots().await;

        let slot_arc = ctrl.slots.get("s").map(|e| e.clone()).expect("slot exists");
        let slot = slot_arc.lock().await;
        assert!(
            matches!(slot.status, SlotStatus::Running { .. }),
            "an Admitting slot whose task is alive must be promoted to Running, got {:?}",
            slot.status
        );

        drop(slot);
        let _ = handle.shutdown().await;
    }

    #[tokio::test]
    async fn recover_reissues_removal_for_terminating_slot_with_alive_task() {
        let (sup, handle, id) = sup_with_live_task().await;
        let ctrl = Controller::new(ControllerConfig::default(), sup.core(), Bus::new(64));

        ctrl.slots.insert(
            Arc::from("s"),
            Arc::new(Mutex::new(SlotState {
                status: SlotStatus::Terminating {
                    cancelled_at: long_ago(),
                },
                running_id: Some(id),
                queue: std::collections::VecDeque::new(),
            })),
        );
        ctrl.running.insert(id, Arc::from("s"));

        ctrl.recover_stale_slots().await;

        let removed = poll_until(Duration::from_secs(2), || async {
            !sup.core().contains_id(id).await
        })
        .await;
        assert!(
            removed,
            "recovery must re-issue the deferred removal for a Terminating slot"
        );

        let _ = handle.shutdown().await;
    }

    #[tokio::test]
    async fn no_queue_advancement_after_shutdown_requested() {
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
        ctrl.handle_event(Arc::new(Event::new(EventKind::ShutdownRequested)))
            .await;
        ctrl.handle_event(Arc::new(
            Event::new(EventKind::TaskRemoved)
                .with_task("occupant")
                .with_id(id),
        ))
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
