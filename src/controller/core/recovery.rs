//! Bus-lag recovery: reconcile slots wedged by a missed `TaskAdded`/`TaskRemoved`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::controller::slot::SlotStatus;
use crate::events::{Event, EventKind};

use super::Controller;

impl Controller {
    /// Recovers slots wedged by a bus lag (a missed `TaskAdded`/`TaskRemoved`).
    pub(super) async fn recover_stale_slots(&self) {
        const RECOVERY_DEADLINE: Duration = Duration::from_secs(5);

        let Some(sup) = self.supervisor.upgrade() else {
            return;
        };

        let slot_keys: Vec<Arc<str>> = self
            .slots
            .iter()
            .map(|entry| Arc::clone(entry.key()))
            .collect();

        for slot_name in slot_keys {
            let Some(slot_arc) = self.slots.get(&*slot_name).map(|e| e.clone()) else {
                continue;
            };
            let mut slot = slot_arc.lock().await;

            if matches!(slot.status, SlotStatus::Idle) {
                continue;
            }
            match slot.status {
                SlotStatus::Admitting { since } if since.elapsed() < RECOVERY_DEADLINE => continue,
                SlotStatus::Terminating { cancelled_at }
                    if cancelled_at.elapsed() < RECOVERY_DEADLINE =>
                {
                    continue;
                }
                _ => {}
            }

            let alive = match slot.running_id {
                Some(rid) => sup.contains_id(rid).await,
                None => false,
            };
            if alive {
                match slot.status {
                    SlotStatus::Admitting { .. } => {
                        slot.status = SlotStatus::Running {
                            started_at: Instant::now(),
                        };
                        self.bus.publish(
                            Event::new(EventKind::ControllerSlotTransition)
                                .with_task(Arc::clone(&slot_name))
                                .with_reason("admitting→running (lag recovery)"),
                        );
                    }
                    SlotStatus::Terminating { .. } => {
                        if let Some(rid) = slot.running_id
                            && let Err(e) = sup.remove(rid)
                        {
                            self.bus.publish(
                                Event::new(EventKind::ControllerRejected)
                                    .with_task(Arc::clone(&slot_name))
                                    .with_reason(format!("recovery_remove_failed: {e}")),
                            );
                        }
                    }
                    _ => {}
                }
                continue;
            }

            if let Some(id) = slot.running_id.take() {
                self.running.remove(&id);
            }
            slot.status = SlotStatus::Idle;

            if !self.is_shutting_down() {
                self.start_next_from_queue(&sup, &mut slot, &slot_name);
            }

            self.gc_if_idle(&slot_name, slot);
        }
    }
}
