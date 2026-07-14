//! Per-slot storage, queue limits, garbage collection, and queue-head replacement.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::{
    controller::slot::SlotState,
    events::{Event, EventKind},
    identity::TaskId,
};

use super::Controller;

impl Controller {
    /// Returns the slot state for `slot_name`, creating an idle slot when absent.
    #[inline]
    pub(super) fn get_or_create_slot(&self, slot_name: &str) -> Arc<Mutex<SlotState>> {
        if let Some(slot) = self.slots.get(slot_name) {
            return slot.clone();
        }
        self.slots
            .entry(Arc::from(slot_name))
            .or_insert_with(|| Arc::new(Mutex::new(SlotState::new())))
            .clone()
    }

    /// Removes an idle, empty slot from the slot map.
    ///
    /// The slot lock is released before removing from the map.
    #[inline]
    pub(super) fn gc_if_idle(
        &self,
        slot_name: &Arc<str>,
        slot: tokio::sync::MutexGuard<'_, SlotState>,
    ) {
        let collect = slot.is_idle() && slot.queue.is_empty();
        drop(slot);
        if collect {
            self.slots.remove(&**slot_name);
        }
    }

    /// Rejects a queued submission if the per-slot queue is already full.
    ///
    /// `slot_len` is the current pending queue depth and does not include the current slot owner.
    #[inline]
    pub(super) fn reject_if_full(&self, slot_name: &str, id: TaskId, slot_len: usize) -> bool {
        if slot_len >= self.config.max_slot_queue() {
            let reason = format!(
                "{}: {}/{}",
                crate::reasons::QUEUE_FULL,
                slot_len,
                self.config.max_slot_queue()
            );
            self.bus.publish(
                Event::new(EventKind::ControllerRejected)
                    .with_task(slot_name)
                    .with_id(id)
                    .with_reason(reason.clone()),
            );
            self.finalize_rejected(id, &reason);
            true
        } else {
            false
        }
    }

    /// Implements latest-wins replacement for the queue head only.
    ///
    /// If the queue has a head, that head is rejected as `superseded_by_replace` and replaced by the new submission.
    /// FIFO items behind it stay in place.
    ///
    /// If the queue is empty, the new submission becomes the head.
    /// This operation does not apply `max_slot_queue`.
    pub(super) fn replace_head_or_push(
        &self,
        slot: &mut SlotState,
        slot_name: &Arc<str>,
        id: TaskId,
        task_spec: crate::TaskSpec,
    ) {
        if let Some(head) = slot.queue.front_mut() {
            let (displaced_id, _) = std::mem::replace(head, (id, task_spec));
            self.bus.publish(
                Event::new(EventKind::ControllerRejected)
                    .with_task(Arc::clone(slot_name))
                    .with_id(displaced_id)
                    .with_reason(crate::reasons::SUPERSEDED_BY_REPLACE),
            );
            self.finalize_rejected(displaced_id, crate::reasons::SUPERSEDED_BY_REPLACE);
        } else {
            slot.queue.push_front((id, task_spec));
        }
    }
}
