//! Pull-introspection: point-in-time snapshot of the controller's slots.

use std::sync::Arc;
use std::time::Duration;

use crate::controller::slot::SlotStatus;
use crate::controller::view::{ControllerSnapshot, SlotStatusKind, SlotView};

use super::Controller;

impl Controller {
    /// Builds a point-in-time [`ControllerSnapshot`] of all tracked slots.
    ///
    /// Slots are sampled one at a time (keys collected first, then each slot locked).
    pub(crate) async fn snapshot(&self) -> ControllerSnapshot {
        let keys: Vec<Arc<str>> = self.slots.iter().map(|e| Arc::clone(e.key())).collect();

        let mut slots = Vec::with_capacity(keys.len());
        for key in keys {
            let Some(slot_arc) = self.slots.get(&*key).map(|e| e.clone()) else {
                continue;
            };
            let slot = slot_arc.lock().await;
            let (status, status_for) = match slot.status {
                SlotStatus::Idle => (SlotStatusKind::Idle, Duration::ZERO),
                SlotStatus::Admitting { since } => (SlotStatusKind::Admitting, since.elapsed()),
                SlotStatus::Running { started_at } => {
                    (SlotStatusKind::Running, started_at.elapsed())
                }
                SlotStatus::Terminating { cancelled_at } => {
                    (SlotStatusKind::Terminating, cancelled_at.elapsed())
                }
            };
            slots.push(SlotView {
                slot: Arc::clone(&key),
                status,
                running: slot.running_id,
                queue_depth: slot.queue.len(),
                status_for,
            });
        }

        slots.sort_by(|a, b| a.slot.cmp(&b.slot));
        ControllerSnapshot { slots }
    }
}
