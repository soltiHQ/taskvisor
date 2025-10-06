//! # Restart policies for task actors.
//!
//! [`RestartPolicy`] determines whether a task should be restarted after it finishes or fails.
//!
//! - [`RestartPolicy::Never`] the task runs once and is never restarted.
//! - [`RestartPolicy::Always`] the task is restarted unconditionally, regardless of exit reason.
//! - [`RestartPolicy::OnFailure`] the task is restarted only if it fails (default).

/// Policy controlling whether a task is restarted after completion or failure.
#[derive(Clone, Copy, Debug)]
pub enum RestartPolicy {
    /// Never restart: the task runs once and exits permanently.
    Never,
    /// Always restart: the task restarts unconditionally after it finishes or fails.
    Always,
    /// Restart only on failure (default).
    OnFailure,
}

impl Default for RestartPolicy {
    /// Returns [`RestartPolicy::OnFailure`].
    fn default() -> Self {
        RestartPolicy::OnFailure
    }
}
