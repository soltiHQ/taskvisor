//! Admission policy handling and slot-owner lifecycle transitions.

use std::sync::Arc;

use tokio::time::Instant;

use crate::core::{OutcomeTx, SupervisorCore, TaskOutcome, deferred_drop::OwnedTask};
use crate::{
    RuntimeError,
    controller::{
        admission::AdmissionPolicy,
        slot::{AdmissionTransition, PendingSubmission, ReplaceAction, SlotPhase, SlotState},
    },
    events::{Event, EventKind, RejectionKind},
    identity::TaskId,
    reasons,
};

use super::{AdmissionResult, CompletionResult, Controller, ControllerWorkers, Submission};

/// Keeps one watched admission owned across controller-side pre-commit work.
///
/// Before parking, the sender is local. After parking, it lives in `Controller::watchers`.
/// A normal queue/registry commit disarms the guard.
/// Unwinding controller-side preparation therefore resolves the waiter as an
/// admission failure instead of leaving it parked.
struct AdmissionWatcher<'a> {
    controller: &'a Controller,
    id: TaskId,
    event_task: Option<Arc<str>>,
    state: AdmissionWatcherState,
    owned: Option<OwnedTask<crate::ControllerSpec>>,
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
        owned: OwnedTask<crate::ControllerSpec>,
        done: Option<OutcomeTx>,
        event_task: Option<Arc<str>>,
    ) -> Self {
        Self {
            controller,
            id,
            event_task,
            state: AdmissionWatcherState::Local(done),
            owned: Some(owned),
        }
    }

    fn take_pending(&mut self, id: TaskId, task_name: Arc<str>) -> PendingSubmission {
        let owned = self
            .owned
            .take()
            .expect("controller ownership is transferred once")
            .map(crate::ControllerSpec::into_task_spec);
        PendingSubmission::new(id, task_name, owned)
    }

    fn dispose_owned(&mut self, terminal: Option<TaskOutcome>) {
        let Some(owned) = self.owned.take() else {
            return;
        };
        let (spec, mut cleanup) = owned.into_parts();
        drop(spec);
        if let Some(terminal) = terminal {
            cleanup.attach_outcome(terminal);
        }
        cleanup.submit();
    }

    /// Moves a watched sender into the controller map before durable admission ownership.
    fn park(&mut self) {
        let state = std::mem::replace(&mut self.state, AdmissionWatcherState::Committed);
        match state {
            AdmissionWatcherState::Local(Some(tx)) => {
                self.controller.state().watchers.insert(self.id, tx);
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
    fn reject(&mut self, kind: RejectionKind, reason: &str) -> Option<TaskOutcome> {
        let state = std::mem::replace(&mut self.state, AdmissionWatcherState::Committed);
        let undelivered = match state {
            AdmissionWatcherState::Local(Some(tx)) => tx
                .send(TaskOutcome::Rejected {
                    kind,
                    reason: Arc::from(reason),
                })
                .err(),
            AdmissionWatcherState::Parked => {
                self.controller.finalize_rejected(self.id, kind, reason)
            }
            AdmissionWatcherState::Local(None) | AdmissionWatcherState::Committed => None,
        };
        if self.owned.is_some() {
            self.dispose_owned(undelivered);
            None
        } else {
            undelivered
        }
    }

    /// Publishes and resolves one controller-side rejection exactly once.
    fn reject_with_event(&mut self, kind: RejectionKind, reason: &str) -> Option<TaskOutcome> {
        if matches!(self.state, AdmissionWatcherState::Committed) {
            return None;
        }
        self.controller.bus.publish_lazy(|| {
            let mut event = Event::new(EventKind::ControllerRejected)
                .with_id(self.id)
                .with_rejection_kind(kind)
                .with_reason(reason);
            if let Some(task) = &self.event_task {
                event = event.with_task(Arc::clone(task));
            }
            event
        });
        self.reject(kind, reason)
    }
}

impl Drop for AdmissionWatcher<'_> {
    fn drop(&mut self) {
        if matches!(self.state, AdmissionWatcherState::Committed) {
            return;
        }
        let undelivered = self.reject_with_event(
            RejectionKind::AdmissionFailed,
            crate::reasons::CONTROLLER_ADMISSION_INTERRUPTED,
        );
        // This guard constructs only `Rejected`, whose fields contain no user-provided destructor.
        drop(undelivered);
    }
}

impl Controller {
    /// Atomically claims one controller-owned registry-capacity payload and watcher.
    pub(super) fn claim_capacity_pending(
        &self,
        id: TaskId,
    ) -> Option<(super::CapacityPending, Option<OutcomeTx>)> {
        let mut state = self.state();
        let waiting = state.capacity_pending.remove(&id)?;
        let done = state.watchers.remove(&id);
        Some((waiting, done))
    }

    /// Rejects a capacity payload already removed from controller indexes.
    fn reject_claimed_capacity(
        &self,
        id: TaskId,
        waiting: super::CapacityPending,
        done: Option<OutcomeTx>,
        kind: RejectionKind,
        reason: &'static str,
    ) {
        self.bus.publish_lazy(|| {
            Event::new(EventKind::ControllerRejected)
                .with_task(Arc::clone(&waiting.slot_name))
                .with_id(id)
                .with_rejection_kind(kind)
                .with_reason(reason)
        });
        let terminal = done.and_then(|done| {
            done.send(TaskOutcome::Rejected {
                kind,
                reason: Arc::from(reason),
            })
            .err()
        });
        self.drop_pending_submission(waiting.pending, terminal);
    }

    /// Returns an immediate busy-slot rejection before pending ownership is created.
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
    /// The immutable task name and effective slot are available before any
    /// controller-owned pending state is created.
    ///
    /// Policy behavior:
    /// - idle slot: enter `Admitting`; commit the registry Add immediately or wait asynchronously for transient registry capacity,
    /// - busy + `Replace`: retire the current owner if needed and keep this submission as the next queued owner,
    /// - busy + `Queue`: append to the slot queue, unless the queue is full,
    /// - busy + `DropIfRunning`: reject immediately.
    ///
    /// A slot becomes `Running` only after the direct registry Add reply succeeds.
    pub(super) async fn handle_submission(&self, sub: Submission, workers: &mut ControllerWorkers) {
        let Submission { id, owned, done } = sub;
        let task_name = owned.value.task_spec().shared_name();
        let admission = owned.value.admission();
        let explicit_slot = owned.value.shared_slot_override();
        let slot_name = explicit_slot
            .as_ref()
            .map_or_else(|| Arc::clone(&task_name), Arc::clone);
        let mut watcher =
            AdmissionWatcher::new(self, id, owned, done, Some(Arc::clone(&slot_name)));
        let Some(sup) = self.supervisor.upgrade() else {
            let _ = watcher.reject_with_event(
                RejectionKind::AdmissionFailed,
                reasons::CONTROLLER_ADMISSION_INTERRUPTED,
            );
            return;
        };

        if self.is_shutting_down() {
            let _ = watcher.reject_with_event(
                RejectionKind::ControllerShuttingDown,
                reasons::CONTROLLER_SHUTTING_DOWN,
            );
            return;
        }

        // Preserve the explicit-slot policy preflight: terminal busy decisions
        // do not need a watcher entry or a pending submission.
        if let Some(explicit_slot) = &explicit_slot
            && let Some(slot_arc) = self.slot(explicit_slot)
        {
            let slot = slot_arc.lock().await;
            if self.is_shutting_down() {
                drop(slot);
                let _ = watcher.reject_with_event(
                    RejectionKind::ControllerShuttingDown,
                    reasons::CONTROLLER_SHUTTING_DOWN,
                );
                return;
            }
            let rejection = self.busy_rejection(&slot, admission);
            drop(slot);
            if let Some((kind, reason)) = rejection {
                let _ = watcher.reject_with_event(kind, &reason);
                return;
            }
        }

        let pending = watcher.take_pending(id, task_name);
        watcher.park();

        let mut displaced_to_drop = None;
        let slot_arc = match self.try_get_or_create_slot(&slot_name) {
            Ok(slot) => slot,
            Err(limit) => {
                let reason = format!("{}: {limit}", reasons::CONTROLLER_SLOT_LIMIT);
                let terminal = watcher.reject_with_event(RejectionKind::ResourceLimit, &reason);
                self.drop_pending_submission(pending, terminal);
                return;
            }
        };
        let mut slot = slot_arc.lock().await;
        if self.is_shutting_down() {
            let terminal = watcher.reject_with_event(
                RejectionKind::ControllerShuttingDown,
                reasons::CONTROLLER_SHUTTING_DOWN,
            );
            self.gc_if_idle(&slot_name, slot);
            self.drop_pending_submission(pending, terminal);
            return;
        }

        match (slot.phase(), admission) {
            (SlotPhase::Idle, _) => {
                match self.start_in_slot(&sup, &mut slot, &slot_name, pending, workers) {
                    Ok(()) => {
                        watcher.commit();
                        let reason: &'static str = match admission {
                            AdmissionPolicy::Queue => "admission=Queue status=admitting",
                            AdmissionPolicy::Replace => "admission=Replace status=admitting",
                            AdmissionPolicy::DropIfRunning => {
                                "admission=DropIfRunning status=admitting"
                            }
                        };
                        self.bus.publish_lazy(|| {
                            Event::new(EventKind::ControllerSubmitted)
                                .with_task(Arc::clone(&slot_name))
                                .with_id(id)
                                .with_reason(reason)
                        });
                    }
                    Err(uncommitted) => {
                        let reason = format!("add_failed: {}", uncommitted.error);
                        let terminal = watcher.reject_with_event(
                            Self::rejection_kind_for_runtime_error(&uncommitted.error),
                            &reason,
                        );
                        self.gc_if_idle(&slot_name, slot);
                        self.drop_start_failure(uncommitted, terminal);
                        return;
                    }
                }
            }
            (SlotPhase::Running { .. }, AdmissionPolicy::Replace) => {
                displaced_to_drop =
                    match self.try_replace_head_or_push(&mut slot, &slot_name, pending) {
                        Ok(displaced) => displaced,
                        Err(pending) => {
                            let reason = self.pending_limit_reason();
                            let terminal =
                                watcher.reject_with_event(RejectionKind::ResourceLimit, &reason);
                            drop(slot);
                            self.drop_pending_submission(*pending, terminal);
                            return;
                        }
                    };
                watcher.commit();
                let ReplaceAction::RemoveNow(owner) = slot.request_replacement(Instant::now())
                else {
                    unreachable!("a running slot must start removal on replace")
                };
                Self::track_removal(
                    &workers.removals,
                    Arc::clone(&sup),
                    owner,
                    Arc::clone(&slot_name),
                );
                self.bus.publish_lazy(|| {
                    Event::new(EventKind::ControllerSlotTransition)
                        .with_task(Arc::clone(&slot_name))
                        .with_reason("running→terminating (replace)")
                });
                self.bus.publish_lazy(|| {
                    Event::new(EventKind::ControllerSubmitted)
                        .with_task(Arc::clone(&slot_name))
                        .with_id(id)
                        .with_reason(format!("admission=Replace depth={}", slot.queue.len()))
                });
            }
            (SlotPhase::Admitting { .. }, AdmissionPolicy::Replace) => {
                displaced_to_drop =
                    match self.try_replace_head_or_push(&mut slot, &slot_name, pending) {
                        Ok(displaced) => displaced,
                        Err(pending) => {
                            let reason = self.pending_limit_reason();
                            let terminal =
                                watcher.reject_with_event(RejectionKind::ResourceLimit, &reason);
                            drop(slot);
                            self.drop_pending_submission(*pending, terminal);
                            return;
                        }
                    };
                watcher.commit();
                let action = slot.request_replacement(Instant::now());
                debug_assert_eq!(action, ReplaceAction::WaitForAdmission);
                self.bus.publish_lazy(|| {
                    Event::new(EventKind::ControllerSlotTransition)
                        .with_task(Arc::clone(&slot_name))
                        .with_reason("admitting→terminating (replace)")
                });
                self.bus.publish_lazy(|| {
                    Event::new(EventKind::ControllerSubmitted)
                        .with_task(Arc::clone(&slot_name))
                        .with_id(id)
                        .with_reason(format!(
                            "admission=Replace status=admitting depth={}",
                            slot.queue.len()
                        ))
                });
            }
            (
                SlotPhase::CancelPendingAdmission { .. } | SlotPhase::Terminating { .. },
                AdmissionPolicy::Replace,
            ) => {
                displaced_to_drop =
                    match self.try_replace_head_or_push(&mut slot, &slot_name, pending) {
                        Ok(displaced) => displaced,
                        Err(pending) => {
                            let reason = self.pending_limit_reason();
                            let terminal =
                                watcher.reject_with_event(RejectionKind::ResourceLimit, &reason);
                            drop(slot);
                            self.drop_pending_submission(*pending, terminal);
                            return;
                        }
                    };
                watcher.commit();
                self.bus.publish_lazy(|| {
                    Event::new(EventKind::ControllerSubmitted)
                        .with_task(Arc::clone(&slot_name))
                        .with_id(id)
                        .with_reason(format!(
                            "admission=Replace status=terminating depth={}",
                            slot.queue.len()
                        ))
                });
            }
            (
                SlotPhase::Admitting { .. }
                | SlotPhase::CancelPendingAdmission { .. }
                | SlotPhase::Running { .. }
                | SlotPhase::Terminating { .. },
                AdmissionPolicy::Queue,
            ) => {
                if let Some(reason) = self.queue_full_reason(slot.queue.len()) {
                    let terminal = watcher.reject_with_event(RejectionKind::QueueFull, &reason);
                    drop(slot);
                    self.drop_pending_submission(pending, terminal);
                    return;
                }
                if let Err(pending) = self.try_push_queued(&mut slot, &slot_name, pending) {
                    let reason = self.pending_limit_reason();
                    let terminal = watcher.reject_with_event(RejectionKind::ResourceLimit, &reason);
                    drop(slot);
                    self.drop_pending_submission(*pending, terminal);
                    return;
                }
                watcher.commit();
                self.bus.publish_lazy(|| {
                    Event::new(EventKind::ControllerSubmitted)
                        .with_task(Arc::clone(&slot_name))
                        .with_id(id)
                        .with_reason(format!("admission=Queue depth={}", slot.queue.len()))
                });
            }
            (
                SlotPhase::Admitting { .. }
                | SlotPhase::CancelPendingAdmission { .. }
                | SlotPhase::Running { .. }
                | SlotPhase::Terminating { .. },
                AdmissionPolicy::DropIfRunning,
            ) => {
                let reason = format!("{} ({})", reasons::DROP_IF_RUNNING, slot.status_label());
                let terminal = watcher.reject_with_event(RejectionKind::SlotBusy, &reason);
                drop(slot);
                self.drop_pending_submission(pending, terminal);
                return;
            }
        }

        drop(slot);
        if let Some(displaced) = displaced_to_drop {
            self.drop_pending_submission(displaced, None);
        }
    }

    /// Applies one authoritative registry registration decision.
    ///
    /// Correlation by both slot and [`TaskId`] makes stale or duplicate results harmless.
    pub(super) async fn handle_admission_result(
        &self,
        result: AdmissionResult,
        workers: &mut ControllerWorkers,
    ) {
        let AdmissionResult {
            id,
            slot_name,
            decision,
        } = result;
        self.handle_registry_decision(id, slot_name, decision, workers)
            .await;
    }

    /// Commits a capacity-waiting payload or rejects it if registry admission closed.
    pub(super) async fn handle_registry_capacity_result(
        &self,
        id: TaskId,
        decision: Result<crate::core::ControllerAddPermit, RuntimeError>,
        workers: &mut ControllerWorkers,
    ) {
        let (slot_name, slot_arc) = {
            let state = self.state();
            let Some(slot_name) = state
                .capacity_pending
                .get(&id)
                .map(|entry| Arc::clone(&entry.slot_name))
            else {
                return;
            };
            let slot_arc = state.slots.get(&*slot_name).cloned();
            (slot_name, slot_arc)
        };
        let Some(slot_arc) = slot_arc else {
            if let Some((waiting, done)) = self.claim_capacity_pending(id) {
                self.reject_claimed_capacity(
                    id,
                    waiting,
                    done,
                    RejectionKind::AdmissionFailed,
                    reasons::CONTROLLER_ADMISSION_INTERRUPTED,
                );
            }
            return;
        };
        let mut slot = slot_arc.lock().await;
        if slot.owner_id() != Some(id) {
            drop(slot);
            if let Some((waiting, done)) = self.claim_capacity_pending(id) {
                self.reject_claimed_capacity(
                    id,
                    waiting,
                    done,
                    RejectionKind::AdmissionFailed,
                    reasons::CONTROLLER_ADMISSION_INTERRUPTED,
                );
            }
            return;
        }
        let Some((waiting, done)) = self.claim_capacity_pending(id) else {
            return;
        };
        debug_assert_eq!(waiting.slot_name, slot_name);
        let PendingSubmission {
            id: pending_id,
            task_name,
            owned,
        } = waiting.pending;
        debug_assert_eq!(pending_id, id);

        let commit = match decision {
            Ok(permit) => match self.supervisor.upgrade() {
                Some(supervisor) => {
                    supervisor.commit_reserved_controller_add(permit, id, task_name, owned, done)
                }
                None => Err(Box::new(crate::core::UncommittedWatchedAdd {
                    error: RuntimeError::ShuttingDown,
                    label: task_name,
                    owned,
                    done,
                })),
            },
            Err(error) => Err(Box::new(crate::core::UncommittedWatchedAdd {
                error,
                label: task_name,
                owned,
                done,
            })),
        };

        match commit {
            Ok((reply, completion)) => {
                Self::track_admission(
                    &workers.admissions,
                    id,
                    Arc::clone(&slot_name),
                    reply,
                    completion,
                );
            }
            Err(mut uncommitted) => {
                if let Some(tx) = uncommitted.done.take() {
                    self.state().watchers.insert(id, tx);
                }
                let shutting_down = matches!(uncommitted.error, RuntimeError::ShuttingDown);
                let (kind, reason) = if shutting_down {
                    (
                        RejectionKind::ControllerShuttingDown,
                        reasons::CONTROLLER_SHUTTING_DOWN.to_owned(),
                    )
                } else {
                    (
                        Self::rejection_kind_for_runtime_error(&uncommitted.error),
                        format!("add_failed: {}", uncommitted.error),
                    )
                };
                self.bus.publish_lazy(|| {
                    Event::new(EventKind::ControllerRejected)
                        .with_task(Arc::clone(&slot_name))
                        .with_id(id)
                        .with_rejection_kind(kind)
                        .with_reason(reason.clone())
                });
                let terminal = self.finalize_rejected(id, kind, &reason);
                let cleared = slot.reject_admission(id);
                debug_assert!(cleared);
                let deferred_drops = if !shutting_down
                    && !self.is_shutting_down()
                    && let Some(supervisor) = self.supervisor.upgrade()
                {
                    self.start_next_from_queue(&supervisor, &mut slot, &slot_name, workers)
                } else {
                    Vec::new()
                };
                self.gc_if_idle(&slot_name, slot);
                self.drop_start_failure(uncommitted, terminal);
                self.drop_pending_submissions(deferred_drops);
            }
        }
    }

    /// Applies one authoritative registry registration decision.
    async fn handle_registry_decision(
        &self,
        id: TaskId,
        slot_name: Arc<str>,
        decision: Result<crate::core::RemovalCompletion, RuntimeError>,
        workers: &mut ControllerWorkers,
    ) {
        let Some(slot_arc) = self.slot(&slot_name) else {
            return;
        };
        let mut slot = slot_arc.lock().await;

        match decision {
            Ok(completion) => match slot.confirm_admission(id, Instant::now()) {
                AdmissionTransition::Running => {
                    Self::track_completion(
                        &workers.completions,
                        id,
                        Arc::clone(&slot_name),
                        completion,
                    );
                    self.bus.publish_lazy(|| {
                        Event::new(EventKind::ControllerSlotTransition)
                            .with_task(Arc::clone(&slot_name))
                            .with_reason("admitting→running")
                    });
                }
                AdmissionTransition::RemoveNow(owner) => {
                    Self::track_completion(
                        &workers.completions,
                        id,
                        Arc::clone(&slot_name),
                        completion,
                    );
                    let Some(sup) = self.supervisor.upgrade() else {
                        return;
                    };
                    Self::track_removal(&workers.removals, sup, owner, slot_name);
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
                    self.start_next_from_queue(&sup, &mut slot, &slot_name, workers)
                } else {
                    Vec::new()
                };

                self.gc_if_idle(&slot_name, slot);
                self.drop_pending_submissions(deferred_drops);
            }
        }
    }

    /// Applies one reliable terminal registry cleanup signal.
    ///
    /// Correlation by slot and [`TaskId`] makes stale or duplicate completions harmless no-ops.
    pub(super) async fn handle_completion_result(
        &self,
        result: CompletionResult,
        workers: &mut ControllerWorkers,
    ) {
        let Some(sup) = self.supervisor.upgrade() else {
            return;
        };
        let Some(slot_arc) = self.slot(&result.slot_name) else {
            return;
        };
        let mut slot = slot_arc.lock().await;
        if !slot.complete_owner(result.id) {
            return;
        }

        let deferred_drops = if !self.is_shutting_down() {
            self.start_next_from_queue(&sup, &mut slot, &result.slot_name, workers)
        } else {
            Vec::new()
        };

        self.gc_if_idle(&result.slot_name, slot);
        self.drop_pending_submissions(deferred_drops);
    }

    /// Hands a submission to the runtime under its pre-minted id.
    ///
    /// The slot enters `Admitting` after an immediate Add commit or after the payload is retained for asynchronous registry-capacity admission.
    /// The watcher stays controller-owned until the Add command commits; non-transient commit failure can therefore resolve it as rejected.
    fn start_in_slot(
        &self,
        sup: &Arc<SupervisorCore>,
        slot: &mut SlotState,
        slot_name: &Arc<str>,
        pending: PendingSubmission,
        workers: &mut ControllerWorkers,
    ) -> Result<(), StartFailure> {
        assert!(slot.is_idle(), "start_in_slot requires an idle slot");
        let PendingSubmission {
            id,
            task_name,
            owned,
        } = pending;
        let done = self.state().watchers.remove(&id);
        match sup.add_task_with_id_watched(id, task_name, owned, done) {
            Ok((reply, completion)) => {
                let started = slot.begin_admission(id, Instant::now());
                debug_assert!(started);
                Self::track_admission(
                    &workers.admissions,
                    id,
                    Arc::clone(slot_name),
                    reply,
                    completion,
                );
                Ok(())
            }
            Err(mut uncommitted) => {
                if let Some(tx) = uncommitted.done.take() {
                    self.state().watchers.insert(id, tx);
                }
                if matches!(uncommitted.error, RuntimeError::CommandQueueFull) {
                    let crate::core::UncommittedWatchedAdd {
                        error: _,
                        label,
                        owned,
                        done,
                    } = *uncommitted;
                    debug_assert!(done.is_none(), "the watcher must remain controller-owned");
                    let waiting = super::CapacityPending {
                        slot_name: Arc::clone(slot_name),
                        pending: PendingSubmission::new(id, label, owned),
                    };
                    if let Err((limit, waiting)) = self.try_index_capacity_pending(id, waiting) {
                        let waiting = *waiting;
                        let PendingSubmission {
                            task_name: label,
                            owned,
                            ..
                        } = waiting.pending;
                        return Err(Box::new(crate::core::UncommittedWatchedAdd {
                            error: RuntimeError::ResourceLimitReached {
                                resource: "controller_pending",
                                limit,
                            },
                            label,
                            owned,
                            done,
                        }));
                    }
                    if let Err(limit) = workers.capacity.enqueue(id) {
                        let waiting = self.unindex_capacity_pending(id);
                        let PendingSubmission {
                            task_name: label,
                            owned,
                            ..
                        } = waiting.pending;
                        return Err(Box::new(crate::core::UncommittedWatchedAdd {
                            error: RuntimeError::ResourceLimitReached {
                                resource: "controller_admission",
                                limit,
                            },
                            label,
                            owned,
                            done,
                        }));
                    }
                    let started = slot.begin_admission(id, Instant::now());
                    debug_assert!(started);
                    Ok(())
                } else {
                    Err(uncommitted)
                }
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
        workers: &mut ControllerWorkers,
    ) -> Vec<(StartFailure, Option<TaskOutcome>)> {
        let mut deferred_drops = Vec::new();
        debug_assert!(slot.is_idle());
        if !slot.is_idle() {
            return deferred_drops;
        }
        while let Some(next) = self.pop_queued_front(slot) {
            let next_id = next.id;
            match self.start_in_slot(sup, slot, slot_name, next, workers) {
                Ok(()) => {
                    self.bus.publish_lazy(|| {
                        Event::new(EventKind::ControllerSubmitted)
                            .with_task(Arc::clone(slot_name))
                            .with_id(next_id)
                            .with_reason(format!("started_from_queue depth={}", slot.queue.len()))
                    });
                    return deferred_drops;
                }
                Err(uncommitted) => {
                    let kind = Self::rejection_kind_for_runtime_error(&uncommitted.error);
                    let reason = format!("queue_start_failed: {}", uncommitted.error);
                    self.bus.publish_lazy(|| {
                        Event::new(EventKind::ControllerRejected)
                            .with_task(Arc::clone(slot_name))
                            .with_id(next_id)
                            .with_rejection_kind(kind)
                            .with_reason(reason.clone())
                    });
                    let terminal = self.finalize_rejected(next_id, kind, &reason);
                    deferred_drops.push((uncommitted, terminal));
                }
            }
        }
        deferred_drops
    }

    /// Drops rejected queue payloads only after their outcomes are terminal and the slot is unlocked.
    pub(super) fn drop_pending_submissions(
        &self,
        pending: Vec<(StartFailure, Option<TaskOutcome>)>,
    ) {
        for (pending, terminal) in pending {
            self.drop_start_failure(pending, terminal);
        }
    }

    /// Submits one rejected controller-owned task to its pre-reserved destructor bundle.
    pub(super) fn dispose_owned_task<T>(&self, owned: OwnedTask<T>, terminal: Option<TaskOutcome>)
    where
        T: Send + 'static,
    {
        let (value, mut cleanup) = owned.into_parts();
        drop(value);
        if let Some(terminal) = terminal {
            cleanup.attach_outcome(terminal);
        }
        cleanup.submit();
    }

    /// Submits one rejected queued task to its pre-reserved destructor bundle.
    pub(super) fn drop_pending_submission(
        &self,
        pending: PendingSubmission,
        terminal: Option<TaskOutcome>,
    ) {
        let PendingSubmission { owned, .. } = pending;
        self.dispose_owned_task(owned, terminal);
    }

    /// Destroys one recovered registry handoff through its existing ownership reservation.
    fn drop_start_failure(&self, pending: StartFailure, terminal: Option<TaskOutcome>) {
        let crate::core::UncommittedWatchedAdd {
            error,
            label,
            owned,
            done,
        } = *pending;
        debug_assert!(done.is_none(), "the watcher must be restored before drop");
        drop((error, label, done));
        self.dispose_owned_task(owned, terminal);
    }

    fn rejection_kind_for_runtime_error(error: &RuntimeError) -> RejectionKind {
        if matches!(error, RuntimeError::ResourceLimitReached { .. }) {
            RejectionKind::ResourceLimit
        } else {
            RejectionKind::AdmissionFailed
        }
    }
}
