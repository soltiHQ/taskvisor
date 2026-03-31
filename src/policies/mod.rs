//! Retry and restart policies.
//!
//! This module groups the knobs that control **if/when** a task is restarted and **how long** to wait between attempts.
//!
//! ## Contents
//! - [`BackoffPolicy`] how retry delays evolve (first / factor / max + jitter)
//! - [`RestartPolicy`] when to restart a task (never / on-failure / always)
//! - [`JitterPolicy`]  randomization strategy to avoid thundering herd
//!
//! ## Wiring
//!
//! All three policies are configured through [`TaskSpec`](crate::TaskSpec).
//!
//! ## Defaults
//! - `BackoffPolicy::default()` first=100ms, factor=1.0 (constant), max=30s, jitter=None.
//! - `JitterPolicy::None` by default; consider `Equal` for balanced randomness.
//! - `RestartPolicy::OnFailure` (recommended default for most tasks).

mod backoff;
pub use backoff::BackoffPolicy;

mod restart;
pub use restart::RestartPolicy;

mod jitter;
pub use jitter::JitterPolicy;
