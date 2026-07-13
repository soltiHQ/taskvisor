//! # Restart policies.
//!
//! [`RestartPolicy`] decides whether a task is started again after an attempt finishes.
//!
//! It answers only the "restart or stop?" question. Retry delay is handled by [`BackoffPolicy`](crate::BackoffPolicy).
//!
//! | Policy                                  | On `Ok(())`                     | On retryable error        |
//! |-----------------------------------------|---------------------------------|---------------------------|
//! | [`Never`](RestartPolicy::Never)         | Stop                            | Stop                      |
//! | [`OnFailure`](RestartPolicy::OnFailure) | Stop                            | Restart with backoff      |
//! | [`Always`](RestartPolicy::Always)       | Restart after optional interval | Restart with backoff      |
//!
//! Fatal errors and cooperative cancellation stop the task for all policies.
//!
//! ## Choosing a policy
//!
//! - Use [`Never`](RestartPolicy::Never) for one-shot tasks.
//! - Use [`OnFailure`](RestartPolicy::OnFailure) for long-running workers.
//! - Use [`Always`](RestartPolicy::Always) for periodic tasks that finish and should run again.

/// Policy controlling whether a task is restarted after an attempt finishes.
///
/// This policy decides restart eligibility. Retry timing is controlled by [`BackoffPolicy`](crate::BackoffPolicy).
///
/// # Also
///
/// - [`BackoffPolicy`](crate::BackoffPolicy) - retry delays after failures
/// - [`TaskSpec`](crate::TaskSpec) - inherits or overrides task execution settings
/// - [`TaskDefaults`](crate::TaskDefaults) - supervisor-wide task settings
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum RestartPolicy {
    /// Run once and never restart.
    ///
    /// The task stops after success, retryable error, fatal error, or cancellation.
    Never,
    /// Restart only after retryable failures.
    ///
    /// A successful completion stops the task. Fatal errors and cooperative
    /// cancellation also stop the task.
    OnFailure,
    /// Restart after success and after retryable failures.
    ///
    /// `interval` applies only after successful completions:
    /// - `Some(dur)` waits at least `dur` before the next run — but never less than the small
    ///   internal floor for an instantly-completing task, so a tiny or zero `dur` cannot hot-loop.
    /// - `None` restarts with no configured delay.
    ///
    /// Instantly-completing tasks are still rate-limited by a small internal floor to avoid a hot restart loop.
    ///
    /// Retryable failures ignore `interval` and use [`BackoffPolicy`](crate::BackoffPolicy).
    /// Fatal errors and cooperative cancellation stop the task.
    ///
    /// This is an input/configuration variant: its field shape is intentionally stable and
    /// remains directly constructible, even though [`RestartPolicy`] itself is non-exhaustive.
    Always {
        /// Wait time between a successful completion and the next run.
        ///
        /// `None` means restart with no configured delay.
        interval: Option<std::time::Duration>,
    },
}

impl Default for RestartPolicy {
    /// Returns [`RestartPolicy::OnFailure`].
    fn default() -> Self {
        RestartPolicy::OnFailure
    }
}
