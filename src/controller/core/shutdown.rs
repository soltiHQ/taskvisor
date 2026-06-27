//! Shutdown drain: resolve every still-pending submission once the loop has stopped.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::core::TaskOutcome;
use crate::events::{Event, EventKind};
use crate::identity::TaskId;

use super::{Controller, Submission};

impl Controller {
    /// Resolves every still-pending watched submission as `Rejected` during shutdown.
    pub(super) fn finalize_pending_on_shutdown(&self, rx: &mut mpsc::Receiver<Submission>) {
        rx.close();

        let pending: Vec<TaskId> = self.watchers.iter().map(|e| *e.key()).collect();
        for id in pending {
            self.finalize_rejected(id, "controller_shutting_down");
        }
        while let Ok(sub) = rx.try_recv() {
            self.bus.publish(
                Event::new(EventKind::ControllerRejected)
                    .with_task(sub.spec.slot_name().to_owned())
                    .with_id(sub.id)
                    .with_reason("controller_shutting_down"),
            );
            if let Some(done) = sub.done {
                let _ = done.send(TaskOutcome::Rejected {
                    reason: Arc::from("controller_shutting_down"),
                });
            }
        }
    }
}
