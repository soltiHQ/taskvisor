//! Pull-side controller introspection.
//!
//! This module builds [`ControllerSnapshot`] from internal slot state.
//! It is used by `SupervisorHandle::controller_snapshot`.

use std::sync::Arc;
use std::time::Duration;

use crate::controller::slot::SlotPhase;
use crate::controller::view::{ControllerSnapshot, SlotStatusKind, SlotView};

use super::Controller;

impl Controller {
    /// Builds a best-effort rolling snapshot of tracked slots.
    ///
    /// The controller captures slot keys, looks each key up again, and then locks each surviving slot separately.
    /// A new slot created after key capture is absent.
    ///
    /// A removed slot may still appear if its `Arc` was cloned before removal.
    /// Each included `SlotView` is internally consistent, but the full collection is not globally atomic.
    ///
    /// The result is sorted by slot key for stable output in tests, logs, and dashboards.
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
