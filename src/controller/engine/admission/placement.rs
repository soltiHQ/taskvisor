//! Applies admission policy to the selected slot.
//!
//! This is the ownership boundary between submission preflight and a slot queue or registry handoff.
//! An idle slot attempts registry handoff regardless of policy.
//! An occupied slot applies `Replace`, `Queue`, or `DropIfRunning`.
//!
//! Placement records durable controller ownership before it reports acceptance.
//! Rejected and displaced values are returned to the caller for cleanup after the slot is unlocked.

use std::sync::Arc;

use tokio::time::Instant;

use crate::{
    controller::policy::AdmissionPolicy,
    core::SupervisorCore,
    events::{Event, EventKind, RejectionKind},
    reasons,
};

use super::{
    super::{
        Controller, TrackedOperations,
        state::{PendingSubmission, ReplaceAction, SlotPhase, SlotState},
    },
    cleanup::StartFailure,
    watcher::AdmissionWatcher,
};

/// Ownership result from applying policy to a locked slot.
pub(super) enum PlacementOutcome {
    /// Queue or admission ownership was committed.
    Accepted {
        /// Replaced queue head to clean up after the slot is unlocked.
        displaced: Option<PendingSubmission>,
    },
    /// The submission was rejected before ownership committed.
    Rejected {
        /// Pending task to clean up after the slot is unlocked.
        pending: PendingSubmission,
        /// Public rejection category.
        kind: RejectionKind,
        /// Diagnostic and watched-outcome reason.
        reason: String,
    },
    /// Runtime handoff returned the task before ownership committed.
    HandoffFailed {
        /// Recovered runtime handoff and its ownership reservation.
        failure: StartFailure,
        /// Public rejection category.
        kind: RejectionKind,
        /// Diagnostic and watched-outcome reason.
        reason: String,
    },
}

/// Controller and runtime context for one locked-slot policy decision.
pub(super) struct SlotPlacement<'a> {
    /// Controller that owns slot state and events.
    controller: &'a Controller,
    /// Runtime used for registration and removal requests.
    supervisor: &'a Arc<SupervisorCore>,
    /// Effective controller slot key.
    slot_name: &'a Arc<str>,
    /// Futures tracked by the serialized controller loop.
    operations: &'a mut TrackedOperations,
}

impl<'a> SlotPlacement<'a> {
    /// Creates a policy context for the selected slot.
    pub(super) fn new(
        controller: &'a Controller,
        supervisor: &'a Arc<SupervisorCore>,
        slot_name: &'a Arc<str>,
        operations: &'a mut TrackedOperations,
    ) -> Self {
        Self {
            controller,
            supervisor,
            slot_name,
            operations,
        }
    }

    /// Applies the selected policy and returns its ownership result.
    pub(super) fn apply(
        &mut self,
        slot: &mut SlotState,
        pending: PendingSubmission,
        admission: AdmissionPolicy,
        watcher: &mut AdmissionWatcher<'_>,
    ) -> PlacementOutcome {
        match (slot.phase(), admission) {
            (SlotPhase::Idle, _) => self.admit_idle(slot, pending, admission, watcher),
            (phase, AdmissionPolicy::Replace) => self.replace(slot, pending, phase, watcher),
            (_, AdmissionPolicy::Queue) => self.queue(slot, pending, watcher),
            (_, AdmissionPolicy::DropIfRunning) => self.reject_busy(slot, pending),
        }
    }

    /// Sends an idle-slot submission to registry handoff.
    fn admit_idle(
        &mut self,
        slot: &mut SlotState,
        pending: PendingSubmission,
        admission: AdmissionPolicy,
        watcher: &mut AdmissionWatcher<'_>,
    ) -> PlacementOutcome {
        let id = pending.id;
        match self.controller.start_in_slot(
            self.supervisor,
            slot,
            self.slot_name,
            pending,
            self.operations,
        ) {
            Ok(()) => {
                watcher.commit();
                let reason = match admission {
                    AdmissionPolicy::Queue => "admission=Queue status=admitting",
                    AdmissionPolicy::Replace => "admission=Replace status=admitting",
                    AdmissionPolicy::DropIfRunning => "admission=DropIfRunning status=admitting",
                };
                self.controller.bus.publish_lazy(|| {
                    Event::new(EventKind::ControllerSubmitted)
                        .with_task(Arc::clone(self.slot_name))
                        .with_id(id)
                        .with_reason(reason)
                });
                PlacementOutcome::Accepted { displaced: None }
            }
            Err(failure) => {
                let kind = Controller::rejection_kind_for_runtime_error(&failure.error);
                let reason = format!("add_failed: {}", failure.error);
                PlacementOutcome::HandoffFailed {
                    failure,
                    kind,
                    reason,
                }
            }
        }
    }

    /// Replaces the queue head and records owner retirement when required.
    fn replace(
        &mut self,
        slot: &mut SlotState,
        pending: PendingSubmission,
        phase: SlotPhase,
        watcher: &mut AdmissionWatcher<'_>,
    ) -> PlacementOutcome {
        let id = pending.id;
        let displaced =
            match self
                .controller
                .try_replace_head_or_push(slot, self.slot_name, pending)
            {
                Ok(displaced) => displaced,
                Err(pending) => {
                    return PlacementOutcome::Rejected {
                        pending: *pending,
                        kind: RejectionKind::ResourceLimit,
                        reason: self.controller.pending_limit_reason(),
                    };
                }
            };
        watcher.commit();

        let reason = match phase {
            SlotPhase::Running { .. } => {
                let ReplaceAction::RemoveNow(owner) = slot.request_replacement(Instant::now())
                else {
                    unreachable!("a running slot must start removal on replace")
                };
                Controller::track_removal(
                    &self.operations.removals,
                    Arc::clone(self.supervisor),
                    owner,
                    Arc::clone(self.slot_name),
                );
                self.publish_transition("running→terminating (replace)");
                format!("admission=Replace depth={}", slot.queue.len())
            }
            SlotPhase::Admitting { .. } => {
                let action = slot.request_replacement(Instant::now());
                debug_assert_eq!(action, ReplaceAction::WaitForAdmission);
                self.publish_transition("admitting→terminating (replace)");
                format!(
                    "admission=Replace status=admitting depth={}",
                    slot.queue.len()
                )
            }
            SlotPhase::CancelPendingAdmission { .. } | SlotPhase::Terminating { .. } => format!(
                "admission=Replace status=terminating depth={}",
                slot.queue.len()
            ),
            SlotPhase::Idle => unreachable!("idle placement uses the direct handoff path"),
        };

        self.publish_submitted(id, reason);
        PlacementOutcome::Accepted { displaced }
    }

    /// Appends one submission within the configured pending limits.
    fn queue(
        &mut self,
        slot: &mut SlotState,
        pending: PendingSubmission,
        watcher: &mut AdmissionWatcher<'_>,
    ) -> PlacementOutcome {
        if let Some(reason) = self.controller.queue_full_reason(slot.queue.len()) {
            return PlacementOutcome::Rejected {
                pending,
                kind: RejectionKind::QueueFull,
                reason,
            };
        }
        let id = pending.id;
        if let Err(pending) = self
            .controller
            .try_push_queued(slot, self.slot_name, pending)
        {
            return PlacementOutcome::Rejected {
                pending: *pending,
                kind: RejectionKind::ResourceLimit,
                reason: self.controller.pending_limit_reason(),
            };
        }
        watcher.commit();
        self.publish_submitted(id, format!("admission=Queue depth={}", slot.queue.len()));
        PlacementOutcome::Accepted { displaced: None }
    }

    /// Rejects a submission because the slot is occupied.
    fn reject_busy(&self, slot: &SlotState, pending: PendingSubmission) -> PlacementOutcome {
        PlacementOutcome::Rejected {
            pending,
            kind: RejectionKind::SlotBusy,
            reason: format!("{} ({})", reasons::DROP_IF_RUNNING, slot.status_label()),
        }
    }

    /// Reports a slot transition while its matching mutation is locked.
    fn publish_transition(&self, reason: &'static str) {
        self.controller.bus.publish_lazy(|| {
            Event::new(EventKind::ControllerSlotTransition)
                .with_task(Arc::clone(self.slot_name))
                .with_reason(reason)
        });
    }

    /// Reports a submission accepted into queue or admission ownership.
    fn publish_submitted(&self, id: crate::TaskId, reason: String) {
        self.controller.bus.publish_lazy(|| {
            Event::new(EventKind::ControllerSubmitted)
                .with_task(Arc::clone(self.slot_name))
                .with_id(id)
                .with_reason(reason)
        });
    }
}
