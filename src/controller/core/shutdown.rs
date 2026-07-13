//! Controller shutdown drain.
//!
//! When the controller loop exits, submissions and control callers may still be waiting for a
//! terminal decision.
//!
//! They can be in two places:
//! - `watchers`: submissions already seen by the controller, with a parked waiter,
//! - `rx`: commands accepted by the controller channel but not processed yet.
//!
//! This module rejects pending submissions and resolves pending control replies as shutting down.

use std::sync::Arc;

use tokio::{sync::mpsc, task::JoinSet};

use crate::RuntimeError;
use crate::core::TaskOutcome;
use crate::events::{Event, EventKind};

use super::{Controller, ControllerCommand};

impl Controller {
    /// Closes the controller command channel and resolves every buffered command.
    ///
    /// This preserves the `submit_and_watch` contract:
    /// a submission that never reaches the runtime must resolve as [`TaskOutcome::Rejected`], not as a dropped oneshot.
    ///
    /// `rx.close()` prevents new messages from being accepted while the remaining buffered submissions are drained.
    pub(super) fn finalize_pending_on_shutdown(&self, rx: &mut mpsc::Receiver<ControllerCommand>) {
        rx.close();

        while let Ok(command) = rx.try_recv() {
            match command {
                ControllerCommand::Submit(sub) => {
                    let mut event = Event::new(EventKind::ControllerRejected)
                        .with_id(sub.id)
                        .with_reason(crate::reasons::CONTROLLER_SHUTTING_DOWN);
                    if let Some(slot_name) = sub.spec.slot_override() {
                        event = event.with_task(slot_name.to_owned());
                    }
                    self.bus.publish(event);

                    if let Some(done) = sub.done {
                        let _ = done.send(TaskOutcome::Rejected {
                            reason: Arc::from(crate::reasons::CONTROLLER_SHUTTING_DOWN),
                        });
                    }
                }
                ControllerCommand::ManageIdentity { reply, .. } => {
                    let _ = reply.send(Err(RuntimeError::ShuttingDown));
                }
            }
        }
    }

    /// Rejects queued slot work, resolves every remaining watcher, and clears slot indexes.
    pub(super) async fn finalize_slot_state_on_shutdown(&self) {
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
            while let Some((id, _spec)) = slot.queue.pop_front() {
                self.bus.publish(
                    Event::new(EventKind::ControllerRejected)
                        .with_task(Arc::clone(&slot_name))
                        .with_id(id)
                        .with_reason(crate::reasons::CONTROLLER_SHUTTING_DOWN),
                );
                self.finalize_rejected(id, crate::reasons::CONTROLLER_SHUTTING_DOWN);
            }
        }

        self.finalize_remaining_watchers();
        self.slots.clear();
    }

    /// Waits until every already-aborted controller worker has finished cancellation.
    pub(super) async fn drain_workers<T: 'static>(workers: &mut JoinSet<T>) {
        while workers.join_next().await.is_some() {}
    }
}
