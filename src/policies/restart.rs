//! # Restart policies for task actors.
//!
//! [`RestartPolicy`] determines whether a task should be restarted after it finishes or fails.
//!
//! - [`RestartPolicy::Never`] the task runs once and is never restarted.
//! - [`RestartPolicy::Always`] the task is restarted unconditionally, with optional delay between successful completions.
//! - [`RestartPolicy::OnFailure`] the task is restarted only if it fails (default).
//!
//! ## Choosing the right policy
//!
//! **One-shot tasks** (run once, exit):
//! ```text
//! RestartPolicy::Never          → Task runs once, exits permanently
//! ```
//!
//! **Periodic tasks** (complete, wait, repeat):
//! ```text
//! RestartPolicy::Always {
//!     interval: Some(Duration)  → Task runs, waits interval, repeats
//! }
//! ```
//!
//! **Long-running tasks** (infinite loop inside):
//! ```text
//! RestartPolicy::OnFailure      → Task crashes → restart with backoff
//! RestartPolicy::Always {
//!     interval: None            → Task exits (success/fail) → restart immediately
//! }
//! ```
//!
//! **Failure recovery**:
//! ```text
//! RestartPolicy::OnFailure      → Restart only on errors (default)
//! ```

/// Policy controlling whether a task is restarted after completion or failure.
#[derive(Clone, Copy, Debug)]
pub enum RestartPolicy {
    /// Never restart: the task runs once and exits permanently.
    Never,
    /// Restart only on failure (default).
    OnFailure,
    /// Always restart: the task restarts unconditionally after it finishes or fails.
    ///   - `interval`: Optional delay between successful completions.
    ///   - `None` → restart immediately after success
    ///   - `Some(dur)` → wait `dur` before next cycle
    Always {
        interval: Option<std::time::Duration>,
    },
}

impl Default for RestartPolicy {
    /// Returns [`RestartPolicy::OnFailure`].
    fn default() -> Self {
        RestartPolicy::OnFailure
    }
}
