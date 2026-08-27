//! Advances an idle slot to the next admissible queued submission.
//!
//! Authoritative rejection, physical completion, or capacity-wait removal can return a slot to `Idle`.
//! Queue order is preserved while candidates cross the registry-handoff boundary.
//! Pre-commit failures retain their values until the slot lock is released for cleanup.

use std::sync::Arc;

use crate::{
    core::{SupervisorCore, TaskOutcome},
    events::{Event, EventKind},
};

use super::{
    super::{Controller, TrackedOperations, state::SlotState},
    cleanup::StartFailure,
};

impl Controller {
    /// Registry handoff for the first admissible queued submission.
    ///
    /// The slot must be idle.
    /// Failed handoffs remain owned by the caller for deferred cleanup.
    pub(in crate::controller::engine) fn start_next_from_queue(
        &self,
        sup: &Arc<SupervisorCore>,
        slot: &mut SlotState,
        slot_name: &Arc<str>,
        operations: &mut TrackedOperations,
    ) -> Vec<(StartFailure, Option<TaskOutcome>)> {
        let mut deferred_drops = Vec::new();
        debug_assert!(slot.is_idle());
        if !slot.is_idle() {
            return deferred_drops;
        }
        while let Some(next) = self.pop_queued_front(slot) {
            let next_id = next.id;
            match self.start_in_slot(sup, slot, slot_name, next, operations) {
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
}
