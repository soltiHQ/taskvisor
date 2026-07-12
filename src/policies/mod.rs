//! # Retry and restart policies.
//!
//! This module contains the policy types that control task restarts.
//!
//! | Type              | Role                                                   |
//! |-------------------|--------------------------------------------------------|
//! | [`RestartPolicy`] | Decides if a task starts again after success or failure |
//! | [`BackoffPolicy`] | Computes retry delay after a retryable failure          |
//! | [`JitterPolicy`]  | Adds random spread to retry delays                      |
//! | [`BackoffError`]  | Error returned by invalid backoff settings              |
//!
//! ## Flow
//!
//! [`TaskSpec`](crate::TaskSpec) stores one [`RestartPolicy`] and one [`BackoffPolicy`].
//! [`BackoffPolicy`] stores the [`JitterPolicy`] used for retry delays.
//!
//! ```text
//! RestartPolicy ─┐
//!                ├──► TaskSpec ──► Supervisor
//! BackoffPolicy ─┘
//!      └── JitterPolicy
//! ```
//!
//! ## Defaults
//!
//! - [`RestartPolicy::OnFailure`] - restart only after retryable failures.
//! - [`BackoffPolicy::default()`] - exponential retry delay starting at `200ms`, capped at `30s`, with equal jitter.
//! - [`JitterPolicy::default()`] - no jitter when the jitter policy is constructed on its own.
//!
//! The default backoff selects [`JitterPolicy::Equal`] explicitly. Named backoff constructors remain deterministic until jitter is added with [`BackoffPolicy::with_jitter`].

mod backoff;
pub use backoff::{BackoffError, BackoffPolicy};

mod restart;
pub use restart::RestartPolicy;

mod jitter;
pub use jitter::JitterPolicy;
