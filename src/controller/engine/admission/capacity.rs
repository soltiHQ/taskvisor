//! Resumes admissions waiting for registry command capacity.
//!
//! When the registry channel is full and admission limits allow a wait,
//! `handoff` stores the task in `ControllerState::capacity_pending`. The
//! controller driver later passes the reserved permit or reservation error here.
//!
//! A permit is committed only while the same [`TaskId`] still owns the slot.
//! A failed matching commit clears that admission, resolves any watcher, and
//! may advance the slot queue.

use std::sync::Arc;

use crate::{
    RuntimeError,
    core::{OutcomeTx, TaskOutcome},
    events::{Event, EventKind, RejectionKind},
    identity::TaskId,
    reasons,
};

use super::super::{
    Controller, TrackedOperations,
    state::{CapacityPending, PendingSubmission},
};

impl Controller {
    /// Removes one capacity waiter and its watched sender in one state update.
    pub(in crate::controller::engine) fn claim_capacity_pending(
        &self,
        id: TaskId,
    ) -> Option<(CapacityPending, Option<OutcomeTx>)> {
        let mut state = self.state();
        let waiting = state.capacity_pending.remove(&id)?;
        let done = state.watchers.remove(&id);
        Some((waiting, done))
    }

    /// Rejects a claimed capacity waiter that cannot continue to the registry.
    fn reject_claimed_capacity(
        &self,
        id: TaskId,
        waiting: CapacityPending,
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

    /// Commits a matching capacity waiter or rejects it before registry handoff.
    ///
    /// A successful commit starts direct registry Add-result tracking.
    pub(in crate::controller::engine) async fn handle_registry_capacity_result(
        &self,
        id: TaskId,
        decision: Result<crate::core::ControllerAddPermit, RuntimeError>,
        operations: &mut TrackedOperations,
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
                    &operations.admissions,
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
                    self.start_next_from_queue(&supervisor, &mut slot, &slot_name, operations)
                } else {
                    Vec::new()
                };
                self.gc_if_idle(&slot_name, slot);
                self.drop_start_failure(uncommitted, terminal);
                self.drop_pending_submissions(deferred_drops);
            }
        }
    }
}
