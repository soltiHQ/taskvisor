//! # taskvisor
//!
//! Taskvisor is a small async task supervisor for Rust.
//!
//! It helps you run named async tasks with:
//!
//! - restart policies,
//! - retry backoff,
//! - per-attempt timeout,
//! - cooperative cancellation,
//! - lifecycle events,
//! - optional final outcomes through `*_and_watch` APIs.
//!
//! The crate is meant to be a building block for services, agents, workers, and higher-level orchestration SDKs.
//!
//! ## Core Model
//!
//! ```text
//! TaskFn / impl Task
//!        |
//!        v
//!     TaskSpec
//!        |
//!        v
//!   Supervisor ---- publishes ----> Event bus ----> Subscribe
//!        |
//!        v
//!    TaskActor
//!   (attempt loop)
//! ```
//!
//! A [`Task`] is the user unit of work.
//! A [`TaskSpec`] configures how that task runs.
//! A [`Supervisor`] owns the runtime and manages task lifecycle.
//! A [`Subscribe`] implementation can observe lifecycle events.
//!
//! ## Task Lifecycle
//!
//! ```text
//! add task
//!   -> TaskStarting
//!   -> task attempt runs
//!   -> TaskStopped / TaskFailed / TimeoutHit / TaskCanceled
//!   -> restart, backoff, or finish
//!   -> TaskRemoved
//! ```
//!
//! Events are best-effort observability.
//! If you need a guaranteed final result for one task, use [`SupervisorHandle::add_and_watch`] or, with the `controller` feature, `submit_and_watch`.
//!
//! ## Main Types
//!
//! | Area     | Types                                                      |
//! |----------|------------------------------------------------------------|
//! | Tasks    | [`Task`], [`TaskFn`], [`TaskContext`], [`TaskSpec`]        |
//! | Runtime  | [`Supervisor`], [`SupervisorHandle`], [`SupervisorConfig`] |
//! | Outcomes | [`TaskWaiter`], [`TaskOutcome`]                            |
//! | Policies | [`RestartPolicy`], [`BackoffPolicy`], [`JitterPolicy`]     |
//! | Events   | [`Event`], [`EventKind`], [`Subscribe`]                    |
//! | Errors   | [`TaskError`], [`RuntimeError`]                            |
//!
//! ## Optional Features
//!
//! - `tokio-util-interop`: exposes raw `tokio_util::sync::CancellationToken` interop through [`TaskContext`] and the prelude.
//! - `controller`: slot-based admission control with `ControllerSpec`, `AdmissionPolicy`, and `ControllerConfig`.
//! - `logging`: built-in `LogWriter` subscriber for examples and simple logs (dev, preview only).
//!
//! ## Quick Start
//!
//! ```rust
//! use taskvisor::prelude::*;
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
//!
//!     let hello: TaskRef = TaskFn::arc("hello", |ctx: TaskContext| async move {
//!         if ctx.is_cancelled() {
//!             return Ok(());
//!         }
//!
//!         println!("Hello from task!");
//!         Ok(())
//!     });
//!
//!     let spec = TaskSpec::once(hello);
//!     sup.run(vec![spec]).await?;
//!
//!     Ok(())
//! }
//! ```

#![forbid(unsafe_code)]

/// Compiles runnable Rust code blocks in `README.md` as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

pub mod prelude;

mod identity;
pub use identity::TaskId;

mod core;
pub use core::{
    Supervisor, SupervisorBuilder, SupervisorConfig, SupervisorHandle, TaskOutcome, TaskWaiter,
};

mod tasks;
pub use tasks::{BoxTaskFuture, Task, TaskContext, TaskFn, TaskRef, TaskSpec};

mod policies;
pub use policies::{BackoffError, BackoffPolicy, JitterPolicy, RestartPolicy};

mod events;
pub use events::{BackoffSource, Event, EventKind};

mod error;
pub use error::{BoxError, Error, RuntimeError, SharedError, TaskError};

mod subscribers;
pub use subscribers::Subscribe;

#[cfg(feature = "controller")]
mod controller;
#[cfg(feature = "controller")]
pub use controller::{
    AdmissionPolicy, ControllerConfig, ControllerError, ControllerSnapshot, ControllerSpec,
    SlotStatusKind, SlotView,
};

#[cfg(feature = "logging")]
pub use subscribers::LogWriter;
