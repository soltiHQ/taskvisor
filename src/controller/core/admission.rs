//! Admission policy handling and slot-owner lifecycle transitions.

use std::sync::Arc;

use tokio::{task::JoinSet, time::Instant};

use crate::core::SupervisorCore;
use crate::{
    RuntimeError, TaskSpec,
    controller::{
        admission::AdmissionPolicy,
        slot::{SlotState, SlotStatus},
    },
    events::{Event, EventKind},
    identity::TaskId,
};

use super::{AdmissionResult, CompletionResult, Controller, RemovalResult, Submission};

impl Controller {
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

    /// Applies one authoritative registry registration decision.
    ///
    /// Correlation by both slot and [`TaskId`] makes a late result harmless after a fast task
    /// has already completed or a different owner has entered the slot.
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
    pub(super) async fn handle_completion_result(
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

    /// Starts the next queued submission, if any.
    ///
    /// Failed starts are rejected and the function continues with the next queued item.
    /// On the first successful start, the slot enters `Admitting`.
    ///
    /// The caller should call this only after the current owner has been cleared.
    pub(super) fn start_next_from_queue(
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
}
