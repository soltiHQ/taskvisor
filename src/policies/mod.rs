//! Controls whether Taskvisor starts another attempt and when it starts.
//!
//! Applications normally select policies through [`TaskSpec`](crate::TaskSpec) or supervisor-wide
//! [`TaskDefaults`](crate::TaskDefaults). Admission resolves those choices once.
//! Taskvisor then applies them after every attempt.
//!
//! ```text
//! TaskSpec + TaskDefaults
//!           │ resolved policy
//!           ▼
//!        task actor
//!           ├── success ──► RestartPolicy ──► stop or repeat
//!           ├── retryable failure ──────────► RestartPolicy + retry limit
//!           │                                          │ retry allowed
//!           │                                          ▼
//!           │                                    BackoffPolicy
//!           │                                          │ base delay
//!           │                                          ▼
//!           │                                    JitterPolicy ──► retry delay
//!           └── fatal or canceled ──► stop
//! ```
//!
//! [`RestartPolicy`] decides whether another attempt is eligible.
//! [`BackoffPolicy`] computes the delay after a retryable failure.
//! [`JitterPolicy`] can spread that delay to avoid synchronized retries.
//!
//! # Choosing a task lifecycle
//!
//! - Use [`TaskSpec::periodic`](crate::TaskSpec::periodic) to repeat after success and allow retries for retryable failures.
//! - Use [`TaskSpec::from_defaults`](crate::TaskSpec::from_defaults) to inherit every runtime default.
//! - Use [`TaskSpec::restartable`](crate::TaskSpec::restartable) to retry retryable failures.
//! - Use [`TaskSpec::once`](crate::TaskSpec::once) for one attempt only.
//!
//! Use [`TaskSpec::with_restart`](crate::TaskSpec::with_restart) for a custom combination.
//! Backoff and the retry limit only affect retryable failures.
//! A periodic success uses its configured interval instead.
//!
//! The built-in defaults use [`RestartPolicy::OnFailure`], exponential backoff from `200ms` to `30s`,
//! equal jitter, no attempt timeout, and no retry-count limit.
//! Named backoff constructors have no jitter unless it is added explicitly.
//!
//! The retry limit counts retries after the first failed attempt in one failure streak.
//! Success resets the count. Fatal errors and cancellation always stop.
//!
//! # Example
//!
//! ```rust
//! use std::num::NonZeroU32;
//! use std::time::Duration;
//! use taskvisor::{BackoffPolicy, JitterPolicy, TaskError, TaskFn, TaskSpec};
//!
//! let task = TaskFn::arc(|_ctx| async {
//!     Err(TaskError::fail("temporary upstream failure"))
//! });
//! let spec = TaskSpec::restartable("api-sync", task)
//!     .with_backoff(
//!         BackoffPolicy::exponential(Duration::from_millis(250))
//!             .with_max(Duration::from_secs(20))
//!             .with_jitter(JitterPolicy::Equal),
//!     )
//!     .with_max_retries(NonZeroU32::new(5).unwrap());
//! ```

mod backoff;
pub use backoff::{BackoffError, BackoffPolicy};

mod restart;
pub use restart::RestartPolicy;

mod jitter;
pub use jitter::JitterPolicy;
