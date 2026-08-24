//! Defines whether Taskvisor may start another attempt.
//!
//! The registry resolves this policy from [`TaskSpec`](crate::TaskSpec) and [`TaskDefaults`](crate::TaskDefaults).
//! Taskvisor checks it after each attempt. Most applications select a policy with [`TaskSpec::once`](crate::TaskSpec::once),
//! [`TaskSpec::restartable`](crate::TaskSpec::restartable), or [`TaskSpec::periodic`](crate::TaskSpec::periodic).
//! Use [`TaskSpec::with_restart`](crate::TaskSpec::with_restart) to override that choice.
//!
//! | Policy      | After success               | After retryable failure |
//! |-------------|-----------------------------|-------------------------|
//! | `Never`     | Stop                        | Stop                    |
//! | `OnFailure` | Stop                        | Retry if budget allows  |
//! | `Always`    | Repeat; use interval if set | Retry if budget allows  |
//!
//! Failure timing belongs to [`BackoffPolicy`](crate::BackoffPolicy).
//! The retry limit can stop an otherwise eligible failure retry.
//! It does not limit successful repeats under `Always`. Fatal errors, task cancellation, and runtime cancellation always stop the task.

/// Restart eligibility applied after one task attempt.
///
/// Failure delays and retry limits are separate settings.
/// This enum is non-exhaustive; include a wildcard arm when matching it.
#[doc(alias = "retry")]
#[doc(alias = "retry policy")]
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum RestartPolicy {
    /// Never schedules another attempt after the current one.
    Never,
    /// Allows another attempt only after a retryable failure.
    ///
    /// The configured retry limit can still stop the task.
    /// Success, fatal failure, and cancellation stop it.
    OnFailure,
    /// Allows another attempt after success or a retryable failure.
    ///
    /// `interval` applies only after a successful attempt:
    ///
    /// - `Some(duration)` waits at least that long after the attempt completes.
    /// - `None` adds no configured interval.
    ///
    /// A small internal floor limits fast successful restart loops, including when `interval` is `None` or zero.
    ///
    /// Retryable failures ignore `interval` and use [`BackoffPolicy`](crate::BackoffPolicy).
    /// The retry limit applies to a failure streak, not to successful repeats.
    /// Fatal errors and cancellation stop the task.
    Always {
        /// Configured wait after success and before the next attempt.
        ///
        /// `None` adds no interval. The fast-loop safety floor still applies.
        interval: Option<std::time::Duration>,
    },
}

impl Default for RestartPolicy {
    /// Returns [`RestartPolicy::OnFailure`].
    fn default() -> Self {
        RestartPolicy::OnFailure
    }
}
