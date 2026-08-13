//! Maps internal slot state to the public snapshot API.
//!
//! [`SupervisorHandle::controller_snapshot`](crate::SupervisorHandle::controller_snapshot)
//! calls [`Controller::snapshot`] on the configured controller.
//! The mapping exposes public status values without exposing slot locks or internal phases.

use std::sync::Arc;
use std::time::Duration;

use crate::controller::{ControllerSnapshot, SlotStatusKind, SlotView};

use super::{Controller, state::SlotPhase};

impl Controller {
    /// Captures tracked slots and returns their public views in slot-key order.
    pub(crate) async fn snapshot(&self) -> ControllerSnapshot {
        let tracked_slots: Vec<_> = {
            let state = self.state();
            state
                .slots
                .iter()
                .map(|(key, slot)| (Arc::clone(key), Arc::clone(slot)))
                .collect()
        };

        let mut slots = Vec::with_capacity(tracked_slots.len());
        for (key, slot_arc) in tracked_slots {
            let slot = slot_arc.lock().await;
            let phase = slot.phase();
            let (status, status_for) = match phase {
                SlotPhase::Idle => (SlotStatusKind::Idle, Duration::ZERO),
                SlotPhase::Admitting { since, .. } => (SlotStatusKind::Admitting, since.elapsed()),
                SlotPhase::Running { started_at, .. } => {
                    (SlotStatusKind::Running, started_at.elapsed())
                }
                SlotPhase::CancelPendingAdmission { requested_at, .. }
                | SlotPhase::Terminating { requested_at, .. } => {
                    (SlotStatusKind::Terminating, requested_at.elapsed())
                }
            };

            slots.push(SlotView {
                slot: Arc::clone(&key),
                status,
                owner_id: phase.owner_id(),
                queue_depth: slot.queue.len(),
                status_for,
            });
        }

        slots.sort_by(|a, b| a.slot.cmp(&b.slot));
        ControllerSnapshot { slots }
    }
}
