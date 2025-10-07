//! Retry and restart policies.
//!
//! This module groups the knobs that control **if/when** a task is restarted
//! and **how long** to wait between attempts.
//!
//! ## Contents
//! - [`RestartPolicy`] when to restart a task (never / on-failure / always)
//! - [`BackoffPolicy`] how retry delays evolve (first / factor / max + jitter)
//! - [`JitterPolicy`]  randomization strategy to avoid thundering herd
//!
//! ## Quick wiring
//! ```text
//! TaskSpec { restart: RestartPolicy, backoff: BackoffPolicy, timeout: Option<Duration> }
//!      └─► core::actor::TaskActor uses:
//!           - restart to decide continue/exit
//!           - backoff.next(prev_delay) to schedule the next attempt
//! ```
//!
//! ## Defaults
//! - `RestartPolicy::OnFailure` (recommended default for most tasks).
//! - `BackoffPolicy::default()` → first=100ms, factor=1.0 (constant), max=30s, jitter=None.
//! - `JitterPolicy::None` by default; consider `Equal` for balanced randomness.

mod backoff;
mod jitter;
mod restart;

pub use backoff::BackoffPolicy;
pub use jitter::JitterPolicy;
pub use restart::RestartPolicy;
