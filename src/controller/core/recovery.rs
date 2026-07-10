//! Controller recovery after bus lag.
//!
//! The controller normally advances slots from runtime events:
//! - `TaskRemoved` frees a slot and starts the next queued submission,
//! - `TaskAddFailed` frees a slot after a failed registry add,
//! - `TaskAdded` promotes `Admitting` to `Running`.
//!
//! The bus is lossy.
//! If the controller listener lags, one of those events may be missed.
//! This module performs a conservative reconciliation pass against the runtime registry.
//!
//! Recovery is delayed and coalesced by the controller loop.
//! This gives retained events time to drain and prevents events emitted during recovery from creating an immediate feedback loop.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use crate::controller::slot::SlotStatus;
use crate::events::{Event, EventKind};

use super::{Controller, schedule_recovery};

/// Delay before controller state is reconciled after event-bus lag.
pub(super) const RECOVERY_DELAY: Duration = Duration::from_secs(5);

impl Controller {
    /// Reconciles stale non-idle slots after the controller misses bus events.
    ///
    /// Returns the earliest deadline for a slot that is still too young to reconcile.
    pub(super) async fn recover_stale_slots(&self) -> Option<Instant> {
        if self.is_shutting_down() {
            return None;
        }

        let sup = self.supervisor.upgrade()?;
        let mut next_recovery = None;

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

            let eligible_at = match slot.status {
                SlotStatus::Admitting { since } => Some(since + RECOVERY_DELAY),
                SlotStatus::Terminating { cancelled_at } => Some(cancelled_at + RECOVERY_DELAY),
                _ => None,
            };
            if let Some(eligible_at) = eligible_at
                && eligible_at > Instant::now()
            {
                let _ = schedule_recovery(&mut next_recovery, eligible_at);
                continue;
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

        next_recovery
    }
}
