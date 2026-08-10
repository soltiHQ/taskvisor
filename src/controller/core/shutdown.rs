//! Controller shutdown drain.
//!
//! When the controller loop exits, submissions and identity callers may still be waiting for results.
//!
//! Pending controller work can include:
//!
//! - `watchers` and slot queues: submissions seen but not handed to the runtime;
//! - `rx`: commands accepted by the channel but not processed;
//! - admission, completion, and removal workers;
//! - identity-operation workers waiting for the registry or terminal cleanup.
//!
//! The lifecycle loop drains buffered commands and slot state, then aborts and drains every worker set.
//!
//! `IdentityReply` guards resolve aborted identity callers as `RuntimeError::ShuttingDown`.
//! Watchers already handed to the registry remain runtime-owned and are resolved there.

use std::sync::Arc;

use tokio::{sync::mpsc, task::JoinSet};

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
                    let super::Submission { id, spec, done } = sub;
                    let mut event = Event::new(EventKind::ControllerRejected)
                        .with_id(id)
                        .with_rejection_kind(RejectionKind::ControllerShuttingDown)
                        .with_reason(crate::reasons::CONTROLLER_SHUTTING_DOWN);
                    if let Some(slot_name) = spec.slot_override() {
                        event = event.with_task(slot_name.to_owned());
                    }
                    self.bus.publish(event);

                    if let Some(done) = done {
                        let _ = done.send(TaskOutcome::Rejected {
                            kind: RejectionKind::ControllerShuttingDown,
                            reason: Arc::from(crate::reasons::CONTROLLER_SHUTTING_DOWN),
                        });
                    }
                    deferred_drops.push(spec);
                }
                ControllerCommand::ManageIdentity { reply, .. } => {
                    let _ = reply.send(Err(RuntimeError::ShuttingDown));
                }
            }
        }

        for spec in deferred_drops {
            self.drop_guarded("drop_buffered_submission", spec).await;
        }
    }

    /// Rejects queued slot work, resolves every remaining watcher, and clears slot indexes.
    pub(super) async fn finalize_slot_state_on_shutdown(&self) {
        let mut pending_to_drop = Vec::new();
        let slot_names: Vec<Arc<str>> = self
            .slots
            .iter()
            .map(|entry| Arc::clone(entry.key()))
            .collect();

        for slot_name in slot_names {
            let Some(slot) = self.slots.get(&*slot_name).map(|entry| entry.clone()) else {
                continue;
            };
            let mut slot = slot.lock().await;
            while let Some(pending) = slot.queue.pop_front() {
                self.bus.publish(
                    Event::new(EventKind::ControllerRejected)
                        .with_task(Arc::clone(&slot_name))
                        .with_id(pending.id)
                        .with_rejection_kind(RejectionKind::ControllerShuttingDown)
                        .with_reason(crate::reasons::CONTROLLER_SHUTTING_DOWN),
                );
                self.finalize_rejected(
                    pending.id,
                    RejectionKind::ControllerShuttingDown,
                    crate::reasons::CONTROLLER_SHUTTING_DOWN,
                );
                pending_to_drop.push(pending);
            }
        }

        self.finalize_remaining_watchers();
        self.slots.clear();
        for pending in pending_to_drop {
            self.drop_guarded("drop_queued_submission", pending).await;
        }
    }

    /// Waits until every already-aborted controller worker has finished cancellation.
    pub(super) async fn drain_workers<T: 'static>(workers: &mut JoinSet<T>) {
        while workers.join_next().await.is_some() {}
    }
}
