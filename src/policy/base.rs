//! # Restart policies for task actors.
//!
//! [`RestartPolicy`] determines whether a task should be restarted after it finishes or fails.
//!
//! # Variants
//! - [`RestartPolicy::Never`] — the task runs once and is never restarted.
//! - [`RestartPolicy::Always`] — the task is restarted unconditionally, regardless of exit reason.
//! - [`RestartPolicy::OnFailure`] — the task is restarted only if it fails (default).
//!
//! # Example
//! ```
//! use taskvisor::RestartPolicy;
//!
//! let p1 = RestartPolicy::Never;
//! let p2 = RestartPolicy::Always;
//! let p3 = RestartPolicy::default();
//!
//! assert!(matches!(p1, RestartPolicy::Never));
//! assert!(matches!(p2, RestartPolicy::Always));
//! assert!(matches!(p3, RestartPolicy::OnFailure));
//! ```

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
