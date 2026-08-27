//! Applies authoritative runtime results to slot state.
//!
//! A committed Add command already owns the task.
//! A successful reply confirms the matching slot owner.
//! An earlier replacement can then require immediate removal.
//!
//! Physical completion is the normal signal that releases a `Running` or `Terminating` owner.
//! Slot name and [`TaskId`] correlation make stale results harmless.

use std::sync::Arc;

use tokio::time::Instant;

use crate::{
    RuntimeError,
    events::{Event, EventKind},
    identity::TaskId,
};

use super::super::{
    AdmissionResult, CompletionResult, Controller, TrackedOperations, state::AdmissionTransition,
};

impl Controller {
    /// Routes one tracked registry Add decision to its slot.
    pub(in crate::controller::engine) async fn handle_admission_result(
        &self,
        result: AdmissionResult,
        operations: &mut TrackedOperations,
    ) {
        let AdmissionResult {
            id,
            slot_name,
            decision,
        } = result;
        self.handle_registry_decision(id, slot_name, decision, operations)
            .await;
    }

    /// Updates a slot from one registry Add decision.
    async fn handle_registry_decision(
        &self,
        id: TaskId,
        slot_name: Arc<str>,
        decision: Result<crate::core::RemovalCompletion, RuntimeError>,
        operations: &mut TrackedOperations,
    ) {
        let Some(slot_arc) = self.slot(&slot_name) else {
            return;
        };
        let mut slot = slot_arc.lock().await;

        match decision {
            Ok(completion) => match slot.confirm_admission(id, Instant::now()) {
                AdmissionTransition::Running => {
                    Self::track_completion(
                        &operations.completions,
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
                        &operations.completions,
                        id,
                        Arc::clone(&slot_name),
                        completion,
                    );
                    let Some(sup) = self.supervisor.upgrade() else {
                        return;
                    };
                    Self::track_removal(&operations.removals, sup, owner, slot_name);
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
                    self.start_next_from_queue(&sup, &mut slot, &slot_name, operations)
                } else {
                    Vec::new()
                };

                self.gc_if_idle(&slot_name, slot);
                self.drop_pending_submissions(deferred_drops);
            }
        }
    }

    /// Releases the matching owner after reliable physical completion.
    pub(in crate::controller::engine) async fn handle_completion_result(
        &self,
        result: CompletionResult,
        operations: &mut TrackedOperations,
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
            self.start_next_from_queue(&sup, &mut slot, &result.slot_name, operations)
        } else {
            Vec::new()
        };

        self.gc_if_idle(&result.slot_name, slot);
        self.drop_pending_submissions(deferred_drops);
    }
}
