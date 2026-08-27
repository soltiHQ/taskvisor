//! Stores the controller task and its shared join result.
//!
//! Runtime shutdown and other join callers share the same stored result.
//! A canceled join wait leaves the `JoinHandle` in place for the next caller.

use tokio::{sync::Mutex, task::JoinHandle};

use crate::events::{Bus, Event};

/// Shared join state for the single controller task.
pub(in crate::controller::engine) struct ControllerTask {
    /// Running task or cached join result.
    state: Mutex<ControllerTaskState>,
}

/// Lifecycle state of the controller task.
enum ControllerTaskState {
    /// Task is still running or waiting to be joined.
    Running(JoinHandle<()>),
    /// Task was joined and stores whether it completed cleanly.
    Joined(bool),
}

impl ControllerTask {
    /// Cancellation-safe shared ownership of the spawned controller task.
    pub(in crate::controller::engine) fn new(handle: JoinHandle<()>) -> Self {
        Self {
            state: Mutex::new(ControllerTaskState::Running(handle)),
        }
    }

    /// Shared join state retained across canceled waits.
    ///
    /// If this future is dropped, a later caller can continue polling the same `JoinHandle`.
    /// `false` means Tokio reported that the controller task did not join cleanly.
    pub(in crate::controller::engine) async fn join(&self, bus: &Bus) -> bool {
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
                bus.publish_lazy(|| {
                    Event::runtime_failure("controller", format!("controller_join_failed: {error}"))
                });
                false
            }
        };
        *state = ControllerTaskState::Joined(clean);
        clean
    }

    #[cfg(test)]
    /// Returns whether the controller task has a cached join result.
    pub(in crate::controller::engine) async fn is_joined(&self) -> bool {
        matches!(*self.state.lock().await, ControllerTaskState::Joined(_))
    }

    #[cfg(test)]
    /// Returns whether a caller currently holds the join-state lock.
    pub(in crate::controller::engine) fn state_is_locked(&self) -> bool {
        self.state.try_lock().is_err()
    }
}
