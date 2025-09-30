//! Runtime core: orchestration and lifecycle.
//!
//! Modules:
//! - [`runner`]: executes one attempt with timeout/cancellation and event publishing.
//! - [`supervisor`]: orchestrates actors, handles shutdown, global concurrency.
//! - [`actor`]: runs a single task with restart policy and backoff.
//! - [`shutdown`]: cross-platform shutdown signal handling.

mod actor;
mod runner;
mod shutdown;
mod supervisor;

pub use supervisor::Supervisor;
