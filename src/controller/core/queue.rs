//! Per-slot storage, queue limits, garbage collection, and queue-head replacement.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::{
    controller::slot::{PendingSubmission, SlotState},
    events::{Event, EventKind, RejectionKind},
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

    /// Builds the rejection reason when the per-slot queue is already full.
    ///
    /// `slot_len` is the current pending queue depth and does not include the current slot owner.
    #[inline]
    pub(super) fn queue_full_reason(&self, slot_len: usize) -> Option<String> {
        if slot_len >= self.config.max_slot_queue() {
            Some(format!(
                "{}: {}/{}",
                crate::reasons::QUEUE_FULL,
                slot_len,
                self.config.max_slot_queue()
            ))
        } else {
            None
        }
    }

    /// Implements latest-wins replacement for the queue head only.
    ///
    /// If the queue has a head, that head is rejected with [`RejectionKind::SupersededByReplace`] and replaced by the new submission.
    /// FIFO items behind it stay in place.
    ///
    /// If the queue is empty, the new submission becomes the head.
    /// This operation does not apply `max_slot_queue`.
    pub(super) fn replace_head_or_push(
        &self,
        slot: &mut SlotState,
        slot_name: &Arc<str>,
        pending: PendingSubmission,
    ) -> Option<PendingSubmission> {
        if let Some(head) = slot.queue.front_mut() {
            let displaced = std::mem::replace(head, pending);
            self.bus.publish(
                Event::new(EventKind::ControllerRejected)
                    .with_task(Arc::clone(slot_name))
                    .with_id(displaced.id)
                    .with_rejection_kind(RejectionKind::SupersededByReplace)
                    .with_reason(crate::reasons::SUPERSEDED_BY_REPLACE),
            );
            self.finalize_rejected(
                displaced.id,
                RejectionKind::SupersededByReplace,
                crate::reasons::SUPERSEDED_BY_REPLACE,
            );
            Some(displaced)
        } else {
            slot.queue.push_front(pending);
            None
        }
    }
}
