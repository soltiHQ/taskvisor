//! # Restart and retry timing
//!
//! These policies answer two separate questions:
//!
//! 1. Should the task run again? See [`RestartPolicy`].
//! 2. If a failed attempt is retried, how long should Taskvisor wait? See [`BackoffPolicy`] and [`JitterPolicy`].
//!
//! ```text
//! attempt returns
//!      │
//!      ├── Ok(()) ────────────────► RestartPolicy
//!      │                              ├── stop
//!      │                              └── wait for Always.interval, then run again
//!      │
//!      ├── Fail / Timeout ────────► RestartPolicy
//!      │                              ├── stop
//!      │                              └── BackoffPolicy ──► wait ──► run again
//!      │
//!      └── Fatal / Canceled ──────► stop
//! ```
//!
//! A [`TaskSpec`](crate::TaskSpec) can set these values for one task.
//! If it does not, it inherits them from [`TaskDefaults`](crate::TaskDefaults).
//!
//! ## Default behavior
//!
//! - [`RestartPolicy::OnFailure`] retries [`TaskError::Fail`](crate::TaskError::Fail) and [`TaskError::Timeout`](crate::TaskError::Timeout).
//! - [`BackoffPolicy::default()`] uses exponential growth from `200ms` to `30s`, with equal jitter.
//! - [`JitterPolicy::default()`] is [`JitterPolicy::None`].
//!
//! The named backoff constructors are deterministic until you add jitter with [`BackoffPolicy::with_jitter`].
//!
//! ## Retry Budget
//!
//! [`TaskSpec::with_max_retries`](crate::TaskSpec::with_max_retries) counts retries after the first failed attempt in one failure streak.
//! A limit of three therefore allows four attempts when every attempt fails.
//! A successful attempt resets the count.
//!
//! Only retryable failures and timeouts use this budget.
//! Fatal errors and cancellation always stop.
//! For [`RestartPolicy::Always`], a successful attempt uses its configured interval; a failed attempt uses the backoff policy.
//! The interval starts after success and is not a wall-clock schedule.

mod backoff;
pub use backoff::{BackoffError, BackoffPolicy};

mod restart;
pub use restart::RestartPolicy;

mod jitter;
pub use jitter::JitterPolicy;
