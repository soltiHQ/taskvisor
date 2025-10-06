//! Runtime core: orchestration and lifecycle.
//!
//! This module contains the embedded implementation of the taskvisor runtime.
//! The only public API from this module is [`Supervisor`], which orchestrates
//! task execution, lifecycle management, and graceful shutdown.
//!
//! Internal modules:
//! - [`runner`]: executes one attempt with timeout/cancellation and event publishing;
//! - [`supervisor`]: orchestrates actors, handles shutdown, global concurrency;
//! - [`actor`]: runs a single task with restart policy and backoff;
//! - [`shutdown`]: cross-platform shutdown signal handling;
//! - [`registry`]: manage task lifecycle.

mod actor;
mod alive;
mod registry;
mod runner;
mod shutdown;
mod supervisor;

pub use supervisor::Supervisor;
