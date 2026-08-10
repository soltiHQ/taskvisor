//! Admission policy handling and slot-owner lifecycle transitions.

use std::sync::Arc;

use tokio::{task::JoinSet, time::Instant};

use crate::core::{OutcomeTx, SupervisorCore, TaskOutcome};
use crate::{
    controller::{
        admission::AdmissionPolicy,
        slot::{AdmissionTransition, PendingSubmission, ReplaceAction, SlotPhase, SlotState},
    },
    events::{Event, EventKind, RejectionKind},
    identity::TaskId,
    reasons,
};

use super::{AdmissionResult, CompletionResult, Controller, RemovalResult, Submission};

/// Keeps one watched admission owned across controller-side pre-commit work.
///
/// Before parking, the sender is local. After parking, it lives in `Controller::watchers`.
/// A normal queue/registry commit disarms the guard.
/// Unwinding user metadata preparation therefore resolves the waiter as an admission failure instead of leaving it parked.
struct AdmissionWatcher<'a> {
    controller: &'a Controller,
    id: TaskId,
    event_task: Option<Arc<str>>,
    state: AdmissionWatcherState,
}

/// Registry handoff that did not commit and therefore remains controller-owned.
type StartFailure = Box<crate::core::UncommittedWatchedAdd>;

enum AdmissionWatcherState {
    Local(Option<OutcomeTx>),
    Parked,
    Committed,
}

impl<'a> AdmissionWatcher<'a> {
    fn new(
        controller: &'a Controller,
        id: TaskId,
        done: Option<OutcomeTx>,
        event_task: Option<Arc<str>>,
    ) -> Self {
        Self {
            controller,
            id,
            event_task,
            state: AdmissionWatcherState::Local(done),
        }
    }

    /// Sets the slot label used by a possible rejection event.
    fn set_event_task(&mut self, task: Arc<str>) {
        self.event_task = Some(task);
    }

    /// Moves a watched sender into the controller map after user metadata is prepared.
    fn park(&mut self) {
        let state = std::mem::replace(&mut self.state, AdmissionWatcherState::Committed);
        match state {
            AdmissionWatcherState::Local(Some(tx)) => {
                self.controller.watchers.insert(self.id, tx);
                self.state = AdmissionWatcherState::Parked;
            }
            AdmissionWatcherState::Local(None) => {
                self.state = AdmissionWatcherState::Parked;
            }
            AdmissionWatcherState::Committed => {}
            AdmissionWatcherState::Parked => {
                self.state = AdmissionWatcherState::Parked;
            }
        }
    }

    /// Marks queue or registry ownership as authoritative.
    fn commit(&mut self) {
        debug_assert!(
            !matches!(self.state, AdmissionWatcherState::Local(Some(_))),
            "a watched admission must be parked before commit"
        );
        self.state = AdmissionWatcherState::Committed;
    }

    /// Resolves a normal controller rejection and disarms unwind fallback.
    fn reject(&mut self, kind: RejectionKind, reason: &str) {
        let state = std::mem::replace(&mut self.state, AdmissionWatcherState::Committed);
        match state {
            AdmissionWatcherState::Local(Some(tx)) => {
                let _ = tx.send(TaskOutcome::Rejected {
                    kind,
                    reason: Arc::from(reason),
                });
            }
            AdmissionWatcherState::Parked => {
                self.controller.finalize_rejected(self.id, kind, reason);
            }
            AdmissionWatcherState::Local(None) | AdmissionWatcherState::Committed => {}
        }
    }

    /// Publishes and resolves one controller-side rejection exactly once.
    fn reject_with_event(&mut self, kind: RejectionKind, reason: &str) {
        if matches!(self.state, AdmissionWatcherState::Committed) {
            return;
        }
        let mut event = Event::new(EventKind::ControllerRejected)
            .with_id(self.id)
            .with_rejection_kind(kind)
            .with_reason(reason);
        if let Some(task) = &self.event_task {
            event = event.with_task(Arc::clone(task));
        }
        self.controller.bus.publish(event);
        self.reject(kind, reason);
    }
}

impl Drop for AdmissionWatcher<'_> {
    fn drop(&mut self) {
        if matches!(self.state, AdmissionWatcherState::Committed) {
            return;
        }
        self.reject_with_event(
            RejectionKind::AdmissionFailed,
            crate::reasons::CONTROLLER_ADMISSION_INTERRUPTED,
        );
    }
}

impl Controller {
    /// Returns an immediate busy-slot rejection that needs no task metadata.
    fn busy_rejection(
        &self,
        slot: &SlotState,
        admission: AdmissionPolicy,
    ) -> Option<(RejectionKind, String)> {
        if slot.is_idle() {
            return None;
        }
        match admission {
            AdmissionPolicy::Queue => self
                .queue_full_reason(slot.queue.len())
                .map(|reason| (RejectionKind::QueueFull, reason)),
            AdmissionPolicy::DropIfRunning => Some((
                RejectionKind::SlotBusy,
                format!("{} ({})", reasons::DROP_IF_RUNNING, slot.status_label()),
            )),
            AdmissionPolicy::Replace => None,
        }
    }

    /// Applies admission policy for one submission.
    ///
    /// Watched submissions are parked in `watchers` until they are either rejected by the controller or handed to the runtime registry.
    /// User-provided task metadata is snapshotted before parking, so a panic from `Task::name` cannot strand the waiter.
    ///
    /// Policy behavior:
    /// - idle slot: try to commit the registry Add; enter `Admitting` on success, otherwise reject the submission,
    /// - busy + `Replace`: retire the current owner if needed and keep this submission as the next queued owner,
    /// - busy + `Queue`: append to the slot queue, unless the queue is full,
    /// - busy + `DropIfRunning`: reject immediately.
    ///
    /// A slot becomes `Running` only after the direct registry Add reply succeeds.
    pub(super) async fn handle_submission(
        &self,
        sub: Submission,
        admissions: &mut JoinSet<AdmissionResult>,
        removals: &mut JoinSet<RemovalResult>,
    ) {
        let Submission { id, spec, done } = sub;
        let admission = spec.admission();
        let explicit_slot = spec.slot_override().map(Arc::<str>::from);
        let mut watcher =
            AdmissionWatcher::new(self, id, done, explicit_slot.as_ref().map(Arc::clone));
        let Some(sup) = self.supervisor.upgrade() else {
            watcher.reject_with_event(
                RejectionKind::AdmissionFailed,
                reasons::CONTROLLER_ADMISSION_INTERRUPTED,
            );
            self.drop_guarded("drop_unavailable_submission", spec).await;
            return;
        };

        if self.is_shutting_down() {
            watcher.reject_with_event(
                RejectionKind::ControllerShuttingDown,
                reasons::CONTROLLER_SHUTTING_DOWN,
            );
            self.drop_guarded("drop_shutdown_submission", spec).await;
            return;
        }

        // An explicit slot is sufficient for policy-only rejection.
        // Avoid calling user-provided `Task::name` when the task cannot be admitted anyway.
        if let Some(slot_name) = &explicit_slot
            && let Some(slot_arc) = self.slots.get(&**slot_name).map(|entry| entry.clone())
        {
            let slot = slot_arc.lock().await;
            if self.is_shutting_down() {
                drop(slot);
                watcher.reject_with_event(
                    RejectionKind::ControllerShuttingDown,
                    reasons::CONTROLLER_SHUTTING_DOWN,
                );
                self.drop_guarded("drop_shutdown_submission", spec).await;
                return;
            }
            let rejection = self.busy_rejection(&slot, admission);
            drop(slot);
            if let Some((kind, reason)) = rejection {
                watcher.reject_with_event(kind, &reason);
                self.drop_guarded("drop_policy_rejected_submission", spec)
                    .await;
                return;
            }
        }

        let Some(task_name) = self
            .guarded("task_name_snapshot", async {
                Arc::<str>::from(spec.task_spec().name())
            })
            .await
        else {
            watcher.reject_with_event(
                RejectionKind::AdmissionFailed,
                reasons::CONTROLLER_ADMISSION_INTERRUPTED,
            );
            self.drop_guarded("drop_name_rejected_submission", spec)
                .await;
            return;
        };
        let slot_name = explicit_slot.unwrap_or_else(|| Arc::clone(&task_name));
        watcher.set_event_task(Arc::clone(&slot_name));
        let pending = PendingSubmission::new(id, task_name, spec.into_task_spec());

        if self.is_shutting_down() {
            watcher.reject_with_event(
                RejectionKind::ControllerShuttingDown,
                reasons::CONTROLLER_SHUTTING_DOWN,
            );
            self.drop_guarded("drop_shutdown_submission", pending).await;
            return;
        }

        watcher.park();

        let mut displaced_to_drop = None;
        let slot_arc = self.get_or_create_slot(&slot_name);
        let mut slot = slot_arc.lock().await;
        if self.is_shutting_down() {
            watcher.reject_with_event(
                RejectionKind::ControllerShuttingDown,
                reasons::CONTROLLER_SHUTTING_DOWN,
            );
            self.gc_if_idle(&slot_name, slot);
            self.drop_guarded("drop_shutdown_submission", pending).await;
            return;
        }

        match (slot.phase(), admission) {
            (SlotPhase::Idle, _) => {
                match self.start_in_slot(&sup, &mut slot, &slot_name, pending, admissions) {
                    Ok(()) => {
                        watcher.commit();
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
                    Err(uncommitted) => {
                        let reason = format!("add_failed: {}", uncommitted.error);
                        watcher.reject_with_event(RejectionKind::AdmissionFailed, &reason);
                        self.gc_if_idle(&slot_name, slot);
                        self.drop_start_failure(uncommitted).await;
                        return;
                    }
                }
            }
            (SlotPhase::Running { .. }, AdmissionPolicy::Replace) => {
                displaced_to_drop = self.replace_head_or_push(&mut slot, &slot_name, pending);
                watcher.commit();
                let ReplaceAction::RemoveNow(owner) = slot.request_replacement(Instant::now())
                else {
                    unreachable!("a running slot must start removal on replace")
                };
                Self::track_removal(removals, Arc::clone(&sup), owner, Arc::clone(&slot_name));
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
            (SlotPhase::Admitting { .. }, AdmissionPolicy::Replace) => {
                displaced_to_drop = self.replace_head_or_push(&mut slot, &slot_name, pending);
                watcher.commit();
                let action = slot.request_replacement(Instant::now());
                debug_assert_eq!(action, ReplaceAction::WaitForAdmission);
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
            (
                SlotPhase::CancelPendingAdmission { .. } | SlotPhase::Terminating { .. },
                AdmissionPolicy::Replace,
            ) => {
                displaced_to_drop = self.replace_head_or_push(&mut slot, &slot_name, pending);
                watcher.commit();
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
                SlotPhase::Admitting { .. }
                | SlotPhase::CancelPendingAdmission { .. }
                | SlotPhase::Running { .. }
                | SlotPhase::Terminating { .. },
                AdmissionPolicy::Queue,
            ) => {
                if let Some(reason) = self.queue_full_reason(slot.queue.len()) {
                    watcher.reject_with_event(RejectionKind::QueueFull, &reason);
                    drop(slot);
                    self.drop_guarded("drop_policy_rejected_submission", pending)
                        .await;
                    return;
                }
                self.push_queued(&mut slot, &slot_name, pending);
                watcher.commit();
                self.bus.publish(
                    Event::new(EventKind::ControllerSubmitted)
                        .with_task(Arc::clone(&slot_name))
                        .with_id(id)
                        .with_reason(format!("admission=Queue depth={}", slot.queue.len())),
                );
            }
            (
                SlotPhase::Admitting { .. }
                | SlotPhase::CancelPendingAdmission { .. }
                | SlotPhase::Running { .. }
                | SlotPhase::Terminating { .. },
                AdmissionPolicy::DropIfRunning,
            ) => {
                let reason = format!("{} ({})", reasons::DROP_IF_RUNNING, slot.status_label());
                watcher.reject_with_event(RejectionKind::SlotBusy, &reason);
                drop(slot);
                self.drop_guarded("drop_policy_rejected_submission", pending)
                    .await;
                return;
            }
        }

        drop(slot);
        if let Some(displaced) = displaced_to_drop {
            self.drop_guarded("drop_superseded_submission", displaced)
                .await;
        }
    }

    /// Applies one authoritative registry registration decision.
    ///
    /// Correlation by both slot and [`TaskId`] makes stale or duplicate results harmless.
    pub(super) async fn handle_admission_result(
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
        let Some(slot_arc) = self.slots.get(&*slot_name).map(|entry| entry.clone()) else {
            return;
        };
        let mut slot = slot_arc.lock().await;

        match decision {
            Ok(completion) => match slot.confirm_admission(id, Instant::now()) {
                AdmissionTransition::Running => {
                    Self::track_completion(completions, id, Arc::clone(&slot_name), completion);
                    self.bus.publish(
                        Event::new(EventKind::ControllerSlotTransition)
                            .with_task(slot_name)
                            .with_reason("admitting→running"),
                    );
                }
                AdmissionTransition::RemoveNow(owner) => {
                    Self::track_completion(completions, id, Arc::clone(&slot_name), completion);
                    let Some(sup) = self.supervisor.upgrade() else {
                        return;
                    };
                    Self::track_removal(removals, sup, owner, slot_name);
                }
                AdmissionTransition::Stale => {}
            },
            Err(_) => {
                if !slot.reject_admission(id) {
                    return;
                }

                let deferred_drops = if !self.is_shutting_down()
                    && let Some(sup) = self.supervisor.upgrade()
                {
                    self.start_next_from_queue(&sup, &mut slot, &slot_name, admissions)
                } else {
                    Vec::new()
                };

                self.gc_if_idle(&slot_name, slot);
                self.drop_pending_submissions(deferred_drops).await;
            }
        }
    }

    /// Applies one reliable terminal registry cleanup signal.
    ///
    /// Correlation by slot and [`TaskId`] makes stale or duplicate completions harmless no-ops.
    pub(super) async fn handle_completion_result(
        &self,
        result: CompletionResult,
        admissions: &mut JoinSet<AdmissionResult>,
    ) {
        let Some(sup) = self.supervisor.upgrade() else {
            return;
        };
        let Some(slot_arc) = self
            .slots
            .get(&*result.slot_name)
            .map(|entry| entry.clone())
        else {
            return;
        };
        let mut slot = slot_arc.lock().await;
        if !slot.complete_owner(result.id) {
            return;
        }

        let deferred_drops = if !self.is_shutting_down() {
            self.start_next_from_queue(&sup, &mut slot, &result.slot_name, admissions)
        } else {
            Vec::new()
        };

        self.gc_if_idle(&result.slot_name, slot);
        self.drop_pending_submissions(deferred_drops).await;
    }

    /// Hands a submission to the runtime under its pre-minted id.
    ///
    /// On successful Add-command commit, the slot enters `Admitting` and the watcher is owned by the runtime registry.
    /// On commit failure, the watcher is put back into `watchers`, the controller can resolve it as rejected instead of dropping the oneshot.
    fn start_in_slot(
        &self,
        sup: &Arc<SupervisorCore>,
        slot: &mut SlotState,
        slot_name: &Arc<str>,
        pending: PendingSubmission,
        admissions: &mut JoinSet<AdmissionResult>,
    ) -> Result<(), StartFailure> {
        assert!(slot.is_idle(), "start_in_slot requires an idle slot");
        let PendingSubmission {
            id,
            task_name,
            task_spec,
        } = pending;
        let done = self.watchers.remove(&id).map(|(_, tx)| tx);
        match sup.add_task_with_id_watched(id, task_name, task_spec, done) {
            Ok((reply, completion)) => {
                let started = slot.begin_admission(id, Instant::now());
                debug_assert!(started);
                Self::track_admission(admissions, id, Arc::clone(slot_name), reply, completion);
                Ok(())
            }
            Err(mut uncommitted) => {
                if let Some(tx) = uncommitted.done.take() {
                    self.watchers.insert(id, tx);
                }
                Err(uncommitted)
            }
        }
    }

    /// Starts the next queued submission, if any.
    ///
    /// Failed Add-command commits are rejected, and the function continues with the next queued item.
    /// After the first successful commit, the slot enters `Admitting` and waits for the direct registry decision.
    ///
    /// The caller should call this only after the current owner has been cleared.
    pub(super) fn start_next_from_queue(
        &self,
        sup: &Arc<SupervisorCore>,
        slot: &mut SlotState,
        slot_name: &Arc<str>,
        admissions: &mut JoinSet<AdmissionResult>,
    ) -> Vec<StartFailure> {
        let mut deferred_drops = Vec::new();
        debug_assert!(slot.is_idle());
        if !slot.is_idle() {
            return deferred_drops;
        }
        while let Some(next) = self.pop_queued_front(slot) {
            let next_id = next.id;
            match self.start_in_slot(sup, slot, slot_name, next, admissions) {
                Ok(()) => {
                    self.bus.publish(
                        Event::new(EventKind::ControllerSubmitted)
                            .with_task(Arc::clone(slot_name))
                            .with_id(next_id)
                            .with_reason(format!("started_from_queue depth={}", slot.queue.len())),
                    );
                    return deferred_drops;
                }
                Err(uncommitted) => {
                    let reason = format!("queue_start_failed: {}", uncommitted.error);
                    self.bus.publish(
                        Event::new(EventKind::ControllerRejected)
                            .with_task(Arc::clone(slot_name))
                            .with_id(next_id)
                            .with_rejection_kind(RejectionKind::AdmissionFailed)
                            .with_reason(reason.clone()),
                    );
                    self.finalize_rejected(next_id, RejectionKind::AdmissionFailed, &reason);
                    deferred_drops.push(uncommitted);
                }
            }
        }
        deferred_drops
    }

    /// Drops rejected queue payloads only after their outcomes are terminal and the slot is unlocked.
    pub(super) async fn drop_pending_submissions(&self, pending: Vec<StartFailure>) {
        for pending in pending {
            self.drop_start_failure(pending).await;
        }
    }

    /// Destroys one recovered registry handoff behind the controller panic boundary.
    async fn drop_start_failure(&self, pending: StartFailure) {
        let crate::core::UncommittedWatchedAdd {
            error,
            label,
            spec,
            done,
        } = *pending;
        debug_assert!(done.is_none(), "the watcher must be restored before drop");
        self.drop_guarded("drop_uncommitted_submission", (error, label, spec, done))
            .await;
    }
}
