//! Controller shutdown drain.
//!
//! When the controller loop exits, some submissions may still be waiting for a terminal decision.
//!
//! They can be in two places:
//! - `watchers`: submissions already seen by the controller, with a parked waiter,
//! - `rx`: submissions accepted by the intake channel but not processed yet.
//!
//! This module resolves both groups as `TaskOutcome::Rejected` with `controller_shutting_down`.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::core::TaskOutcome;
use crate::events::{Event, EventKind};
use crate::identity::TaskId;

use super::{Controller, Submission};

impl Controller {
    /// Resolves every still-pending watched submission during controller shutdown.
    ///
    /// This preserves the `submit_and_watch` contract:
    /// a submission that never reaches the runtime must resolve as [`TaskOutcome::Rejected`], not as a dropped oneshot.
    ///
    /// The drain is split into two parts:
    /// - already parked watchers are rejected through [`finalize_rejected`](Self::finalize_rejected),
    /// - not-yet-processed intake submissions are drained from `rx` and rejected directly.
    ///
    /// `rx.close()` prevents new messages from being accepted while the remaining buffered submissions are drained.
    pub(super) fn finalize_pending_on_shutdown(&self, rx: &mut mpsc::Receiver<Submission>) {
        rx.close();

        let pending: Vec<TaskId> = self.watchers.iter().map(|e| *e.key()).collect();
        for id in pending {
            self.bus.publish(
                Event::new(EventKind::ControllerRejected)
                    .with_id(id)
                    .with_reason(crate::reasons::CONTROLLER_SHUTTING_DOWN),
            );
            self.finalize_rejected(id, crate::reasons::CONTROLLER_SHUTTING_DOWN);
        }

        while let Ok(sub) = rx.try_recv() {
            self.bus.publish(
                Event::new(EventKind::ControllerRejected)
                    .with_task(sub.spec.slot_name().to_owned())
                    .with_id(sub.id)
                    .with_reason(crate::reasons::CONTROLLER_SHUTTING_DOWN),
            );

            if let Some(done) = sub.done {
                let _ = done.send(TaskOutcome::Rejected {
                    reason: Arc::from(crate::reasons::CONTROLLER_SHUTTING_DOWN),
                });
            }
        }
    }
}
