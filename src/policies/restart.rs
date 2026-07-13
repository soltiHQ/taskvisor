//! # When a task runs again
//!
//! [`RestartPolicy`] decides if Taskvisor starts another attempt.
//! It does not decide the delay after a failure; [`BackoffPolicy`](crate::BackoffPolicy) does that.
//!
//! | Policy                                  | On `Ok(())`                     | On retryable error        |
//! |-----------------------------------------|---------------------------------|---------------------------|
//! | [`Never`](RestartPolicy::Never)         | Stop                            | Stop                      |
//! | [`OnFailure`](RestartPolicy::OnFailure) | Stop                            | Restart with backoff      |
//! | [`Always`](RestartPolicy::Always)       | Restart after optional interval | Restart with backoff      |
//!
//! [`TaskError::Fatal`](crate::TaskError::Fatal) and [`TaskError::Canceled`](crate::TaskError::Canceled) always stop the task.
//!
//! ## Choosing a policy
//!
//! - Use [`Never`](RestartPolicy::Never) for one-shot tasks.
//! - Use [`OnFailure`](RestartPolicy::OnFailure) for long-running workers.
//! - Use [`Always`](RestartPolicy::Always) for periodic tasks that finish and should run again.

/// Decides whether a task starts another attempt.
///
/// This policy decides restart eligibility. Retry timing is controlled by [`BackoffPolicy`](crate::BackoffPolicy).
/// Include a wildcard arm when matching because new policies may be added.
///
/// # Also
///
/// - [`BackoffPolicy`](crate::BackoffPolicy) - retry delays after failures
/// - [`TaskSpec`](crate::TaskSpec) - inherits or overrides task execution settings
/// - [`TaskDefaults`](crate::TaskDefaults) - supervisor-wide task settings
#[doc(alias = "retry")]
#[doc(alias = "retry policy")]
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum RestartPolicy {
    /// Run once and never restart.
    ///
    /// The task stops after its first attempt, whatever result it returns.
    Never,
    /// Restart only after retryable failures.
    ///
    /// Success, fatal failure, and cancellation stop the task.
    OnFailure,
    /// Restart after success and after retryable failures.
    ///
    /// `interval` applies only after a successful attempt:
    ///
    /// - `Some(duration)` waits at least that long after the attempt completes.
    /// - `None` has no configured wait.
    ///
    /// Taskvisor also limits very fast successful restart loops.
    /// This safety limit applies even when `interval` is `None` or zero.
    ///
    /// Retryable failures ignore `interval` and use [`BackoffPolicy`](crate::BackoffPolicy).
    /// Fatal errors and cooperative cancellation stop the task.
    Always {
        /// Wait time between a successful completion and the next run.
        ///
        /// `None` means there is no configured wait. The fast-loop safety limit still applies.
        interval: Option<std::time::Duration>,
    },
}

impl Default for RestartPolicy {
    /// Returns [`RestartPolicy::OnFailure`].
    fn default() -> Self {
        RestartPolicy::OnFailure
    }
}
