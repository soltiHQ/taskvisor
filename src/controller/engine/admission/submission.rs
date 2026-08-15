//! Starts admission for each ordered controller submission.
//!
//! The lifecycle driver calls this module after a `Submit` command leaves the controller queue.
//! It reads the immutable task name, admission policy, and effective slot before converting
//! `ControllerSpec` into pending `TaskSpec` ownership.
//!
//! Preflight handles shutdown and immediate rejection. The watched outcome is then parked in controller state
//! before `placement` applies policy to the locked slot. Local rejection is resolved before user values enter cleanup.

use std::sync::Arc;

use crate::{controller::policy::AdmissionPolicy, events::RejectionKind, reasons};

use super::{
    super::{Controller, Submission, TrackedOperations, state::SlotState},
    placement::{PlacementOutcome, SlotPlacement},
    watcher::AdmissionWatcher,
};

impl Controller {
    /// Returns the policy rejection available for an occupied slot.
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

    /// Checks an existing explicit slot before pending ownership is created.
    async fn explicit_slot_rejection(
        &self,
        explicit_slot: Option<&Arc<str>>,
        admission: AdmissionPolicy,
    ) -> Option<(RejectionKind, String)> {
        let slot_name = explicit_slot?;
        let slot_ref = self.slot(slot_name)?;
        let slot = slot_ref.lock().await;
        if self.is_shutting_down() {
            return Some((
                RejectionKind::ControllerShuttingDown,
                reasons::CONTROLLER_SHUTTING_DOWN.to_owned(),
            ));
        }
        self.busy_rejection(&slot, admission)
    }

    /// Runs one ordered submission through preflight and slot placement.
    pub(in crate::controller::engine) async fn handle_submission(
        &self,
        sub: Submission,
        operations: &mut TrackedOperations,
    ) {
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

        if let Some((kind, reason)) = self
            .explicit_slot_rejection(explicit_slot.as_ref(), admission)
            .await
        {
            let _ = watcher.reject_with_event(kind, &reason);
            return;
        }

        let pending = watcher.take_pending(id, task_name);
        watcher.park();

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

        let outcome = SlotPlacement::new(self, &sup, &slot_name, operations).apply(
            &mut slot,
            pending,
            admission,
            &mut watcher,
        );
        match outcome {
            PlacementOutcome::Accepted { displaced } => {
                drop(slot);
                if let Some(displaced) = displaced {
                    self.drop_pending_submission(displaced, None);
                }
            }
            PlacementOutcome::Rejected {
                pending,
                kind,
                reason,
            } => {
                let terminal = watcher.reject_with_event(kind, &reason);
                drop(slot);
                self.drop_pending_submission(pending, terminal);
            }
            PlacementOutcome::HandoffFailed {
                failure,
                kind,
                reason,
            } => {
                let terminal = watcher.reject_with_event(kind, &reason);
                self.gc_if_idle(&slot_name, slot);
                self.drop_start_failure(failure, terminal);
            }
        }
    }
}
