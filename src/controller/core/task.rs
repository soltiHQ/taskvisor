//! Lifecycle wrapper for the single spawned controller task.

use tokio::{sync::Mutex, task::JoinHandle};

use crate::events::{Bus, Event, EventKind};

pub(super) struct ControllerTask {
    state: Mutex<ControllerTaskState>,
}

enum ControllerTaskState {
    Running(JoinHandle<()>),
    Joined(bool),
}

impl ControllerTask {
    pub(super) fn new(handle: JoinHandle<()>) -> Self {
        Self {
            state: Mutex::new(ControllerTaskState::Running(handle)),
        }
    }

    /// Joins the stored task without taking it out of shared state before the await.
    ///
    /// If this future is dropped, a later caller can continue polling the same `JoinHandle`.
    /// Returns `false` when Tokio reports that the controller task did not join cleanly.
    pub(super) async fn join(&self, bus: &Bus) -> bool {
        let mut state = self.state.lock().await;
        if let ControllerTaskState::Joined(clean) = &*state {
            return *clean;
        }
        let ControllerTaskState::Running(handle) = &mut *state else {
            unreachable!("joined controller state was returned above")
        };

        let clean = match handle.await {
            Ok(()) => true,
            Err(error) => {
                bus.publish(
                    Event::new(EventKind::ControllerRejected)
                        .with_task("controller")
                        .with_reason(format!("controller_join_failed: {error}")),
                );
                false
            }
        };
        *state = ControllerTaskState::Joined(clean);
        clean
    }

    #[cfg(test)]
    pub(super) async fn is_joined(&self) -> bool {
        matches!(*self.state.lock().await, ControllerTaskState::Joined(_))
    }

    #[cfg(test)]
    pub(super) fn state_is_locked(&self) -> bool {
        self.state.try_lock().is_err()
    }
}
