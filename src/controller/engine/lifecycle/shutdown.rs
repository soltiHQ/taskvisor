//! Resolves controller-owned work when the loop stops.
//!
//! After tracked operations are dropped, the driver drains buffered commands,
//! queued submissions, capacity waiters, and controller-owned outcome senders.
//! Pending identity calls and watched controller-owned submissions receive an
//! explicit shutdown or rejection result.
//! Submissions already handed to the registry remain runtime-owned.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::RuntimeError;
use crate::core::TaskOutcome;
use crate::events::{Event, EventKind, RejectionKind};

use super::super::{Controller, ControllerCommand, Submission};

impl Controller {
    /// Closes command intake and resolves every buffered command.
    ///
    /// Buffered watched submissions resolve as [`TaskOutcome::Rejected`].
    /// Identity commands resolve with `RuntimeError::ShuttingDown`.
    pub(in crate::controller::engine) async fn finalize_pending_on_shutdown(
        &self,
        rx: &mut mpsc::Receiver<ControllerCommand>,
    ) {
        rx.close();
        let mut deferred_drops = Vec::new();

        while let Ok(command) = rx.try_recv() {
            match command {
                ControllerCommand::Submit(sub) => {
                    let Submission { id, owned, done } = *sub;
                    let event_task = owned
                        .value
                        .shared_slot_override()
                        .unwrap_or_else(|| owned.value.task_spec().shared_name());
                    self.bus.publish_lazy(|| {
                        Event::new(EventKind::ControllerRejected)
                            .with_id(id)
                            .with_rejection_kind(RejectionKind::ControllerShuttingDown)
                            .with_reason(crate::reasons::CONTROLLER_SHUTTING_DOWN)
                            .with_task(event_task)
                    });

                    let terminal = done.and_then(|done| {
                        done.send(TaskOutcome::Rejected {
                            kind: RejectionKind::ControllerShuttingDown,
                            reason: Arc::from(crate::reasons::CONTROLLER_SHUTTING_DOWN),
                        })
                        .err()
                    });
                    deferred_drops.push((owned, terminal));
                }
                ControllerCommand::ManageIdentity { reply, .. } => {
                    let _ = reply.send(Err(RuntimeError::ShuttingDown));
                }
            }
        }

        for (owned, terminal) in deferred_drops {
            self.dispose_owned_task(owned, terminal);
        }
    }

    /// Rejects queued slot work, resolves remaining watchers, and clears indexes.
    pub(in crate::controller::engine) async fn finalize_slot_state_on_shutdown(&self) {
        let mut pending_to_drop = Vec::new();
        let capacity_waiting: Vec<_> = {
            let mut state = self.state();
            state.capacity_pending.drain().collect()
        };
        for (id, waiting) in capacity_waiting {
            self.bus.publish_lazy(|| {
                Event::new(EventKind::ControllerRejected)
                    .with_task(Arc::clone(&waiting.slot_name))
                    .with_id(id)
                    .with_rejection_kind(RejectionKind::ControllerShuttingDown)
                    .with_reason(crate::reasons::CONTROLLER_SHUTTING_DOWN)
            });
            let terminal = self.finalize_rejected(
                id,
                RejectionKind::ControllerShuttingDown,
                crate::reasons::CONTROLLER_SHUTTING_DOWN,
            );
            pending_to_drop.push((waiting.pending, terminal));
        }
        let slots: Vec<_> = {
            let state = self.state();
            state
                .slots
                .iter()
                .map(|(name, slot)| (Arc::clone(name), Arc::clone(slot)))
                .collect()
        };

        for (slot_name, slot) in slots {
            let mut slot = slot.lock().await;
            while let Some(pending) = self.pop_queued_front(&mut slot) {
                self.bus.publish_lazy(|| {
                    Event::new(EventKind::ControllerRejected)
                        .with_task(Arc::clone(&slot_name))
                        .with_id(pending.id)
                        .with_rejection_kind(RejectionKind::ControllerShuttingDown)
                        .with_reason(crate::reasons::CONTROLLER_SHUTTING_DOWN)
                });
                let terminal = self.finalize_rejected(
                    pending.id,
                    RejectionKind::ControllerShuttingDown,
                    crate::reasons::CONTROLLER_SHUTTING_DOWN,
                );
                pending_to_drop.push((pending, terminal));
            }
        }

        self.finalize_remaining_watchers();
        {
            let mut state = self.state();
            state.slots.clear();
            state.queued_slots.clear();
            state.capacity_pending.clear();
        }
        for (pending, terminal) in pending_to_drop {
            self.drop_pending_submission(pending, terminal);
        }
    }
}
