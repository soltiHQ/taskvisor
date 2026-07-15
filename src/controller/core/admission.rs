//! Admission policy handling and slot-owner lifecycle transitions.

use std::sync::Arc;

use tokio::{task::JoinSet, time::Instant};

use crate::core::SupervisorCore;
use crate::{
    RuntimeError, TaskSpec,
    controller::{
        admission::AdmissionPolicy,
        slot::{AdmissionTransition, ReplaceAction, SlotPhase, SlotState},
    },
    events::{Event, EventKind, RejectionKind},
    identity::TaskId,
    reasons,
};

use super::{AdmissionResult, CompletionResult, Controller, RemovalResult, Submission};

impl Controller {
    /// Applies admission policy for one submission.
    ///
    /// Watched submissions are parked in `watchers` until they are either rejected by the controller or handed to the runtime registry.
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
                    .with_rejection_kind(RejectionKind::ControllerShuttingDown)
                    .with_reason(crate::reasons::CONTROLLER_SHUTTING_DOWN),
            );
            self.finalize_rejected(
                id,
                RejectionKind::ControllerShuttingDown,
                crate::reasons::CONTROLLER_SHUTTING_DOWN,
            );
            return;
        }

        let slot_name: Arc<str> = Arc::from(spec.slot_name());
        let admission = spec.admission();
        let task_spec = spec.into_task_spec();

        let slot_arc = self.get_or_create_slot(&slot_name);
        let mut slot = slot_arc.lock().await;
        if self.is_shutting_down() {
            self.bus.publish(
                Event::new(EventKind::ControllerRejected)
                    .with_task(Arc::clone(&slot_name))
                    .with_id(id)
                    .with_rejection_kind(RejectionKind::ControllerShuttingDown)
                    .with_reason(crate::reasons::CONTROLLER_SHUTTING_DOWN),
            );
            self.finalize_rejected(
                id,
                RejectionKind::ControllerShuttingDown,
                crate::reasons::CONTROLLER_SHUTTING_DOWN,
            );
            return;
        }

        match (slot.phase(), admission) {
            (SlotPhase::Idle, _) => {
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
                                .with_rejection_kind(RejectionKind::AdmissionFailed)
                                .with_reason(reason.clone()),
                        );
                        self.finalize_rejected(id, RejectionKind::AdmissionFailed, &reason);
                        self.gc_if_idle(&slot_name, slot);
                    }
                }
            }
            (SlotPhase::Running { .. }, AdmissionPolicy::Replace) => {
                self.replace_head_or_push(&mut slot, &slot_name, id, task_spec);
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
                self.replace_head_or_push(&mut slot, &slot_name, id, task_spec);
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
                SlotPhase::Admitting { .. }
                | SlotPhase::CancelPendingAdmission { .. }
                | SlotPhase::Running { .. }
                | SlotPhase::Terminating { .. },
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
                SlotPhase::Admitting { .. }
                | SlotPhase::CancelPendingAdmission { .. }
                | SlotPhase::Running { .. }
                | SlotPhase::Terminating { .. },
                AdmissionPolicy::DropIfRunning,
            ) => {
                let reason = format!("{} ({})", reasons::DROP_IF_RUNNING, slot.status_label());
                self.bus.publish(
                    Event::new(EventKind::ControllerRejected)
                        .with_task(Arc::clone(&slot_name))
                        .with_id(id)
                        .with_rejection_kind(RejectionKind::SlotBusy)
                        .with_reason(reason.clone()),
                );
                self.finalize_rejected(id, RejectionKind::SlotBusy, &reason);
            }
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

        if !self.is_shutting_down() {
            self.start_next_from_queue(&sup, &mut slot, &result.slot_name, admissions);
        }

        self.gc_if_idle(&result.slot_name, slot);
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
        id: TaskId,
        task_spec: TaskSpec,
        admissions: &mut JoinSet<AdmissionResult>,
    ) -> Result<(), RuntimeError> {
        assert!(slot.is_idle(), "start_in_slot requires an idle slot");
        let done = self.watchers.remove(&id).map(|(_, tx)| tx);
        match sup.add_task_with_id_watched(id, task_spec, done) {
            Ok((reply, completion)) => {
                let started = slot.begin_admission(id, Instant::now());
                debug_assert!(started);
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
    ) {
        debug_assert!(slot.is_idle());
        if !slot.is_idle() {
            return;
        }
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
                            .with_rejection_kind(RejectionKind::AdmissionFailed)
                            .with_reason(reason.clone()),
                    );
                    self.finalize_rejected(next_id, RejectionKind::AdmissionFailed, &reason);
                }
            }
        }
    }
}
