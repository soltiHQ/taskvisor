//! Policies for supervising task execution:
//!  - [`BackoffPolicy`] for retry delays;
//!  - [`RestartPolicy`] for restart decisions.
//!
//! ## Overview
//! - [`backoff`] — compute the next delay after failures with a capped multiplicative growth.
//! - [`restart`] — decide whether to restart a task: *never / always / on failure*.

mod backoff;
mod restart;

pub use backoff::BackoffPolicy;
pub use restart::RestartPolicy;
