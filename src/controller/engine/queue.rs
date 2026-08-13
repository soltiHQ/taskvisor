//! Keeps slot queues and controller indexes consistent.
//!
//! Admission code uses these helpers to update a slot queue and its reverse task index in the same serialized transition.
//! This module also enforces slot and pending limits, replaces queue heads, and removes unused slots.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::{
    events::{Event, EventKind, RejectionKind},
    identity::TaskId,
};

use super::{
    CapacityPending, Controller,
    state::{PendingSubmission, SlotState},
};

impl Controller {
    /// Records ownership of one submission after it enters a slot queue.
    #[inline]
    fn index_queued(&self, id: TaskId, slot_name: &Arc<str>) {
        let previous = self.state().queued_slots.insert(id, Arc::clone(slot_name));
        debug_assert!(
            previous.is_none(),
            "a controller TaskId cannot belong to two slot queues"
        );
    }

    /// Removes one submission from the reverse index as it leaves queue ownership.
    #[inline]
    fn unindex_queued(&self, id: TaskId) {
        self.state().queued_slots.remove(&id);
    }

    /// Appends one submission and updates the reverse index in the same controller transition.
    #[inline]
    #[cfg(test)]
    pub(super) fn push_queued(
        &self,
        slot: &mut SlotState,
        slot_name: &Arc<str>,
        pending: PendingSubmission,
    ) {
        let id = pending.id;
        slot.queue.push_back(pending);
        self.index_queued(id, slot_name);
    }

    /// Appends one submission when the aggregate pending budget has capacity.
    #[inline]
    pub(super) fn try_push_queued(
        &self,
        slot: &mut SlotState,
        slot_name: &Arc<str>,
        pending: PendingSubmission,
    ) -> Result<(), Box<PendingSubmission>> {
        let id = pending.id;
        {
            let mut state = self.state();
            if let Some(limit) = self.config.max_total_pending()
                && state.pending_len() >= limit.get()
            {
                return Err(Box::new(pending));
            }
            let previous = state.queued_slots.insert(id, Arc::clone(slot_name));
            debug_assert!(previous.is_none());
        }
        slot.queue.push_back(pending);
        Ok(())
    }

    /// Pops the next submission and removes its reverse-index entry.
    #[inline]
    pub(super) fn pop_queued_front(&self, slot: &mut SlotState) -> Option<PendingSubmission> {
        let pending = slot.queue.pop_front()?;
        self.unindex_queued(pending.id);
        Some(pending)
    }

    /// Removes a known queue position and its reverse-index entry.
    #[inline]
    pub(super) fn remove_queued_at(
        &self,
        slot: &mut SlotState,
        position: usize,
    ) -> Option<PendingSubmission> {
        let pending = slot.queue.remove(position)?;
        self.unindex_queued(pending.id);
        Some(pending)
    }

    /// Returns the slot state for `slot_name`, creating an idle slot when absent.
    #[inline]
    #[cfg(test)]
    pub(super) fn get_or_create_slot(&self, slot_name: &str) -> Arc<Mutex<SlotState>> {
        let mut state = self.state();
        if let Some(slot) = state.slots.get(slot_name) {
            return slot.clone();
        }
        state
            .slots
            .entry(Arc::from(slot_name))
            .or_insert_with(|| Arc::new(Mutex::new(SlotState::new())))
            .clone()
    }

    /// Returns an existing slot or creates one without exceeding the aggregate slot budget.
    ///
    /// The serialized controller loop performs the limit check and insertion as one controller-state transition.
    #[inline]
    pub(super) fn try_get_or_create_slot(
        &self,
        slot_name: &Arc<str>,
    ) -> Result<Arc<Mutex<SlotState>>, usize> {
        let mut state = self.state();
        if let Some(slot) = state.slots.get(slot_name.as_ref()) {
            return Ok(slot.clone());
        }
        if let Some(limit) = self.config.max_controller_slots() {
            let limit = limit.get();
            if state.slots.len() >= limit {
                return Err(limit);
            }
        }
        Ok(state
            .slots
            .entry(Arc::clone(slot_name))
            .or_insert_with(|| Arc::new(Mutex::new(SlotState::new())))
            .clone())
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
            self.state().slots.remove(&**slot_name);
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

    /// Atomically validates and indexes one registry-capacity waiter under the aggregate budget.
    #[inline]
    pub(super) fn try_index_capacity_pending(
        &self,
        id: TaskId,
        pending: CapacityPending,
    ) -> Result<(), (usize, Box<CapacityPending>)> {
        let mut state = self.state();
        if let Some(limit) = self.config.max_total_pending()
            && state.pending_len() >= limit.get()
        {
            return Err((limit.get(), Box::new(pending)));
        }
        let previous = state.capacity_pending.insert(id, pending);
        debug_assert!(previous.is_none());
        Ok(())
    }

    /// Rolls back a capacity index before the controller transition becomes externally visible.
    #[inline]
    pub(super) fn unindex_capacity_pending(&self, id: TaskId) -> CapacityPending {
        self.state()
            .capacity_pending
            .remove(&id)
            .expect("the capacity waiter was indexed by this transition")
    }

    /// Builds the aggregate pending-limit rejection reason from the current controller state.
    #[inline]
    pub(super) fn pending_limit_reason(&self) -> String {
        match self.config.max_total_pending() {
            Some(limit) => {
                let state = self.state();
                format!(
                    "{}: {}/{}",
                    crate::reasons::CONTROLLER_PENDING_LIMIT,
                    state.pending_len(),
                    limit.get()
                )
            }
            None => crate::reasons::CONTROLLER_PENDING_LIMIT.to_owned(),
        }
    }

    /// Replaces the queue head with the latest submission.
    ///
    /// An existing head is rejected with [`RejectionKind::SupersededByReplace`].
    /// The remaining queue keeps its order. An empty queue receives the new head without applying `max_slot_queue`.
    pub(super) fn replace_head_or_push(
        &self,
        slot: &mut SlotState,
        slot_name: &Arc<str>,
        pending: PendingSubmission,
    ) -> Option<PendingSubmission> {
        let pending_id = pending.id;
        if let Some(head) = slot.queue.front_mut() {
            let mut displaced = std::mem::replace(head, pending);
            {
                let mut state = self.state();
                state.queued_slots.remove(&displaced.id);
                let previous = state.queued_slots.insert(pending_id, Arc::clone(slot_name));
                debug_assert!(previous.is_none());
            }
            self.bus.publish_lazy(|| {
                Event::new(EventKind::ControllerRejected)
                    .with_task(Arc::clone(slot_name))
                    .with_id(displaced.id)
                    .with_rejection_kind(RejectionKind::SupersededByReplace)
                    .with_reason(crate::reasons::SUPERSEDED_BY_REPLACE)
            });
            let terminal = self.finalize_rejected(
                displaced.id,
                RejectionKind::SupersededByReplace,
                crate::reasons::SUPERSEDED_BY_REPLACE,
            );
            if let Some(terminal) = terminal {
                displaced.owned.cleanup.attach_outcome(terminal);
            }
            Some(displaced)
        } else {
            slot.queue.push_front(pending);
            self.index_queued(pending_id, slot_name);
            None
        }
    }

    /// Implements latest-wins head replacement while enforcing aggregate pending depth.
    ///
    /// Replacing a head does not consume pending capacity.
    /// Inserting into an empty queue fails when the aggregate limit is exhausted.
    pub(super) fn try_replace_head_or_push(
        &self,
        slot: &mut SlotState,
        slot_name: &Arc<str>,
        pending: PendingSubmission,
    ) -> Result<Option<PendingSubmission>, Box<PendingSubmission>> {
        if slot.queue.is_empty() {
            let id = pending.id;
            let mut state = self.state();
            if let Some(limit) = self.config.max_total_pending()
                && state.pending_len() >= limit.get()
            {
                return Err(Box::new(pending));
            }
            slot.queue.push_front(pending);
            let previous = state.queued_slots.insert(id, Arc::clone(slot_name));
            debug_assert!(previous.is_none());
            return Ok(None);
        }
        Ok(self.replace_head_or_push(slot, slot_name, pending))
    }
}
