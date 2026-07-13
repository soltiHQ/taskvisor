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
    /// Builds a point-in-time snapshot of currently tracked slots.
    ///
    /// This snapshot is not globally atomic.
    /// The controller first collects slot keys, then locks each slot one by one.
    /// A slot may be removed between those two steps; such slots are skipped.
    ///
    /// The result is sorted by slot key for stable output in tests, logs, and dashboards.
    pub(crate) async fn snapshot(&self) -> ControllerSnapshot {
        let keys: Vec<Arc<str>> = self.slots.iter().map(|e| Arc::clone(e.key())).collect();

        let mut slots = Vec::with_capacity(keys.len());
        for key in keys {
            let Some(slot_arc) = self.slots.get(&*key).map(|e| e.clone()) else {
                continue;
            };

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
                running: phase.owner_id(),
                queue_depth: slot.queue.len(),
                status_for,
            });
        }

        slots.sort_by(|a, b| a.slot.cmp(&b.slot));
        ControllerSnapshot { slots }
    }
}
