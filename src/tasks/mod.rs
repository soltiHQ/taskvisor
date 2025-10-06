//! Tasks: trait, function-backed implementation, and execution spec.
//!
//! This module groups the public task abstractions used by the runtime:
//!
//! - [`Task`] — async, cancelable unit of work (trait).
//! - [`TaskFn`] — function-backed implementation that wraps closures.
//! - [`TaskSpec`] — execution specification (restart/backoff/timeout).
//!
//! ## Overview
//! ```text
//! Task (trait)  ◄── implemented by user code or TaskFn
//!     ▲
//!     │ wraps
//! TaskFn (closure → Future per spawn)
//!
//! TaskSpec (what/how to run) ──► Supervisor/Registry/Actor
//! ```
//!
//! ## Example
//! ```rust
//! use std::time::Duration;
//! use tokio_util::sync::CancellationToken;
//! use taskvisor::{TaskFn, TaskRef, TaskSpec, RestartPolicy, BackoffPolicy};
//!
//! let t: TaskRef = TaskFn::arc("hello", |_ctx: CancellationToken| async move {
//!     // do work...
//!     Ok::<(), taskvisor::TaskError>(())
//! });
//!
//! let spec = TaskSpec::new(
//!     t,
//!     RestartPolicy::Never,
//!     BackoffPolicy::default(),
//!     Some(Duration::from_secs(5)),
//! );
//! # let _ = spec; 
//! ```

mod r#impl;
mod spec;
mod task;

pub use r#impl::func::TaskFn;
pub use spec::TaskSpec;
pub use task::{Task, TaskRef};
