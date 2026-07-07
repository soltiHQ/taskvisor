//! # taskvisor
//!
//! Taskvisor is a task supervisor for Tokio: it restarts background tasks on failure and reports every step through typed events.
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
//! | Errors   | [`Error`], [`TaskError`], [`RuntimeError`]                 |
//!
//! All main types are re-exported at the crate root.
//! For the concept behind each area, read the module pages:
//! [`tasks`], [`policies`], [`events`], [`subscribers`], [`core`], [`identity`], and `controller` (feature-gated).
//!
//! ## Optional Features
//!
//! - `tokio-util-interop`: exposes raw `tokio_util::sync::CancellationToken` interop through [`TaskContext`] and the prelude.
//! - `controller`:         slot-based admission control with `ControllerSpec`, `AdmissionPolicy`, and `ControllerConfig`.
//! - `tracing`:            built-in `TracingBridge` subscriber that forwards runtime events to the `tracing` ecosystem.
//! - `logging`:            built-in `LogWriter` subscriber for examples and simple logs (dev, preview only).
//! - `test-util`:          test helpers ([`TaskContext::detached`], [`TaskId::for_tests`], `TaskOutcome::*_for_tests`).
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
//!     let hello: TaskRef = TaskFn::arc("hello", |_ctx| async move {
//!         println!("Hello from task!");
//!         Ok(())
//!     });
//!
//!     sup.run(vec![TaskSpec::once(hello)]).await?;
//!     Ok(())
//! }
//! ```
//!
//! ## Examples
//!
//! All examples run as-is, from simple to advanced
//! ([browse them on GitHub](https://github.com/soltiHQ/taskvisor/tree/main/examples)):
//!
//! | Example | What it shows |
//! |---------------------------------------------------------------------------------------------|-------------------------------------------------------------------|
//! | [basic](https://github.com/soltiHQ/taskvisor/blob/main/examples/basic.rs)                   | Run one task and exit — the minimal wiring                        |
//! | [worker](https://github.com/soltiHQ/taskvisor/blob/main/examples/worker.rs)                 | A long-running worker that stops cleanly on Ctrl+C                |
//! | [periodic](https://github.com/soltiHQ/taskvisor/blob/main/examples/periodic.rs)             | Run a job every N seconds, forever                                |
//! | [multiple](https://github.com/soltiHQ/taskvisor/blob/main/examples/multiple.rs)             | Several tasks with different restart rules under one supervisor   |
//! | [queue_consumer](https://github.com/soltiHQ/taskvisor/blob/main/examples/queue_consumer.rs) | A message consumer that reconnects after failures                 |
//! | [cpu_job](https://github.com/soltiHQ/taskvisor/blob/main/examples/cpu_job.rs)               | Run CPU-heavy work on rayon, supervised, without blocking Tokio   |
//! | [subscriber](https://github.com/soltiHQ/taskvisor/blob/main/examples/subscriber.rs)         | React to lifecycle events with your own handler                   |
//! | [tracing](https://github.com/soltiHQ/taskvisor/blob/main/examples/tracing.rs)               | Send supervisor events into your logs (feature `tracing`)         |
//! | [metrics](https://github.com/soltiHQ/taskvisor/blob/main/examples/metrics.rs)               | Count lifecycle events as Prometheus metrics                      |
//! | [dynamic](https://github.com/soltiHQ/taskvisor/blob/main/examples/dynamic.rs)               | Add, cancel, and remove tasks while the app is running            |
//! | [outcomes](https://github.com/soltiHQ/taskvisor/blob/main/examples/outcomes.rs)             | Wait for a task's final result: done, failed, or canceled         |
//! | [slots](https://github.com/soltiHQ/taskvisor/blob/main/examples/slots.rs)                   | Limit concurrency per slot: queue, replace, or drop the newcomer  |
//! | [admission](https://github.com/soltiHQ/taskvisor/blob/main/examples/admission.rs)           | Find out if your submission ran or was rejected                   |

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

/// Compiles runnable Rust code blocks in `README.md` as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

pub mod prelude;

pub mod identity;
pub use identity::TaskId;

pub mod reasons;

pub mod core;
pub use core::{
    Supervisor, SupervisorBuilder, SupervisorConfig, SupervisorHandle, TaskOutcome, TaskWaiter,
};

pub mod tasks;
pub use tasks::{BoxTaskFuture, Task, TaskContext, TaskFn, TaskRef, TaskSpec};

pub mod policies;
pub use policies::{BackoffError, BackoffPolicy, JitterPolicy, RestartPolicy};

pub mod error;
pub use error::{BoxError, Error, RuntimeError, SharedError, TaskError};

pub mod events;
pub use events::{BackoffSource, Event, EventKind};

pub mod subscribers;
pub use subscribers::Subscribe;

#[cfg(feature = "controller")]
pub mod controller;
#[cfg(feature = "controller")]
pub use controller::{
    AdmissionPolicy, ControllerConfig, ControllerError, ControllerSnapshot, ControllerSpec,
    SlotStatusKind, SlotView,
};

#[cfg(feature = "logging")]
pub use subscribers::LogWriter;

#[cfg(feature = "tracing")]
pub use subscribers::TracingBridge;
