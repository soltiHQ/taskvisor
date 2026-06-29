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
//! - [`BackoffPolicy::default()`] - constant `100ms` retry delay, capped at `30s`.
//! - [`JitterPolicy::None`] - no random delay by default.
//!
//! For balanced retry spreading, use [`JitterPolicy::Equal`].

mod backoff;
pub use backoff::{BackoffError, BackoffPolicy};

mod restart;
pub use restart::RestartPolicy;

mod jitter;
pub use jitter::JitterPolicy;
