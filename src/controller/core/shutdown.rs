//! Controller shutdown drain.
//!
//! When the controller loop exits, submissions and identity callers may still be waiting for results.
//!
//! Pending controller work can include:
//!
//! - metadata workers, `watchers`, and slot queues: submissions seen but not handed to the runtime;
//! - `rx`: commands accepted by the channel but not processed;
//! - admission, completion, and removal workers;
//! - identity-operation workers waiting for the registry or terminal cleanup.
//!
//! The lifecycle loop drains buffered commands and slot state, then drops every in-loop worker future.
//!
//! `IdentityReply` guards resolve aborted identity callers as `RuntimeError::ShuttingDown`.
//! Watchers already handed to the registry remain runtime-owned and are resolved there.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::RuntimeError;
use crate::core::TaskOutcome;
use crate::events::{Event, EventKind, RejectionKind};

use super::{Controller, ControllerCommand};

impl Controller {
    /// Closes the controller command channel and resolves every buffered command.
    ///
    /// This preserves the `submit_and_watch` contract:
    /// a submission that never reaches the runtime must resolve as [`TaskOutcome::Rejected`], not as a dropped oneshot.
    ///
    /// `rx.close()` prevents new messages from being accepted while the remaining buffered commands are drained.
    pub(super) async fn finalize_pending_on_shutdown(
        &self,
        rx: &mut mpsc::Receiver<ControllerCommand>,
    ) {
        rx.close();
        let mut deferred_drops = Vec::new();

        while let Ok(command) = rx.try_recv() {
            match command {
                ControllerCommand::Submit(sub) => {
                    let super::Submission { id, owned, done } = *sub;
                    self.bus.publish_lazy(|| {
                        let mut event = Event::new(EventKind::ControllerRejected)
                            .with_id(id)
                            .with_rejection_kind(RejectionKind::ControllerShuttingDown)
                            .with_reason(crate::reasons::CONTROLLER_SHUTTING_DOWN);
                        if let Some(slot_name) = owned.value.slot_override() {
                            event = event.with_task(slot_name.to_owned());
                        }
                        event
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

    /// Rejects queued slot work, resolves every remaining watcher, and clears slot indexes.
    pub(super) async fn finalize_slot_state_on_shutdown(&self) {
        let mut pending_to_drop = Vec::new();
        let (metadata_waiting, metadata_ready): (Vec<_>, Vec<_>) = {
            let mut state = self.state();
            state.metadata_order.clear();
            (
                state.metadata_pending.drain().collect(),
                std::mem::take(&mut state.metadata_ready)
                    .into_values()
                    .collect(),
            )
        };
        for (id, pending) in metadata_waiting {
            pending.cancel.cancel();
            self.bus.publish_lazy(|| {
                let mut event = Event::new(EventKind::ControllerRejected)
                    .with_id(id)
                    .with_rejection_kind(RejectionKind::ControllerShuttingDown)
                    .with_reason(crate::reasons::CONTROLLER_SHUTTING_DOWN);
                if let Some(task) = pending.event_task {
                    event = event.with_task(task);
                }
                event
            });
            let terminal = self.finalize_rejected(
                id,
                RejectionKind::ControllerShuttingDown,
                crate::reasons::CONTROLLER_SHUTTING_DOWN,
            );
            drop(terminal);
        }
        drop(metadata_ready);
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
            state.metadata_pending.clear();
            state.metadata_order.clear();
            state.metadata_ready.clear();
            state.queued_slots.clear();
            state.capacity_pending.clear();
        }
        for (pending, terminal) in pending_to_drop {
            self.drop_pending_submission(pending, terminal);
        }
    }
}
