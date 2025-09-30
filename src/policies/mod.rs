//! Policies for supervising task execution:
//!  - [`BackoffPolicy`] for retry delays;
//!  - [`RestartPolicy`] for restart decisions.
//!
//! ## Overview
//! - [`backoff`] — compute the next delay after failures with a capped multiplicative growth.
//! - [`restart`] — decide whether to restart a task: *never / always / on failure*.
//!
//! ## Example
//! ```rust
//! use taskvisor::{BackoffPolicy, RestartPolicy};
//! use std::time::Duration;
//!
//! // A task that fails 3 times, then succeeds.
//! struct Task { attempts: usize }
//! impl Task {
//!     fn run(&mut self) -> Result<(), &'static str> {
//!         self.attempts += 1;
//!         if self.attempts <= 3 { Err("fail") } else { Ok(()) }
//!     }
//! }
//!
//! // Backoff: 1ms -> 2ms -> 4ms (capped by 8ms)
//! let backoff = BackoffPolicy {
//!     first:  Duration::from_millis(1),
//!     max:    Duration::from_millis(8),
//!     factor: 2.0,
//! };
//! let policy = RestartPolicy::OnFailure;
//!
//! let mut t = Task { attempts: 0 };
//! let mut prev_delay: Option<Duration> = None;
//! let mut delays: Vec<Duration> = Vec::new();
//!
//! loop {
//!     match t.run() {
//!         Ok(()) => break,
//!         Err(_) => {
//!             if matches!(policy, RestartPolicy::Never) { break; }
//!             let d = backoff.next(prev_delay);
//!             delays.push(d);
//!             prev_delay = Some(d);
//!         }
//!     }
//! }
//!
//! assert_eq!(t.attempts, 4);
//! assert_eq!(delays, vec![
//!     Duration::from_millis(1),
//!     Duration::from_millis(2),
//!     Duration::from_millis(4),
//! ]);
//! ```

mod backoff;
mod restart;

pub use backoff::BackoffPolicy;
pub use restart::RestartPolicy;