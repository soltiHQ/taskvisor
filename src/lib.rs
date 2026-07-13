//! # taskvisor
//!
//! Taskvisor supervises long-running Tokio tasks. It can restart failed work,
//! slow down retries, time out each attempt, and stop tasks during shutdown.
//!
//! Use it for workers, consumers, connection loops, and other background work
//! that should have a clear lifecycle.
//!
//! ## Start Here
//!
//! 1. Create a [`Task`] with [`TaskFn`] or your own type.
//! 2. Wrap it in a [`TaskSpec`] to choose restart rules.
//! 3. Start a [`Supervisor`] in static or dynamic mode.
//! 4. Make long-running tasks listen to [`TaskContext`] cancellation.
//!
//! ```text
//! TaskFn or impl Task
//!          |
//!          v
//!       TaskSpec  -- fills missing values from --> TaskDefaults
//!          |
//!          v
//!      Supervisor
//!       |       |
//!       |       +-- best-effort events --> Subscribe
//!       |
//!       +-- watched final result -------> TaskWaiter
//! ```
//!
//! ## Choose a Mode
//!
//! - **Static:** call [`Supervisor::run`] with a known task set. It waits for
//!   all tasks to finish or for an OS shutdown signal.
//! - **Dynamic:** call [`Supervisor::serve`]. It returns a
//!   [`SupervisorHandle`] that can add, remove, cancel, and list tasks while the
//!   service is running.
//!
//! `run` registers its initial task list as one batch. If a name is repeated or
//! already in use, no task from that batch starts.
//!
//! ## One Task, Many Attempts
//!
//! A registered task has one [`TaskId`], but it may run several attempts:
//!
//! ```text
//! register
//!    |
//!    v
//! attempt 1 -- failure --> backoff --> attempt 2 -- success --> finish
//!                                                       |
//!                                                       v
//!                                                 TaskOutcome
//! ```
//!
//! Attempts for one task never overlap. [`RestartPolicy`] decides whether a new
//! attempt is allowed. [`BackoffPolicy`] sets the delay after a retryable
//! failure. A timeout applies to one attempt, not to the full task lifetime.
//!
//! ## Cancellation and Shutdown
//!
//! Cancellation is cooperative first. A long-running task should await
//! [`TaskContext::cancelled`] or use [`TaskContext::run_until_cancelled`], then
//! return [`TaskError::Canceled`]. During shutdown, taskvisor waits for the
//! configured grace period. It aborts tasks that still have not stopped.
//!
//! Dropping one supervisor or handle clone does not stop the runtime. Dropping
//! the last public owner sends best-effort cancellation, but cannot wait for
//! cleanup. Call [`SupervisorHandle::shutdown`] when cleanup must be complete
//! before your code continues.
//!
//! ## Events or Final Outcomes?
//!
//! Lifecycle [`Event`] values are **best-effort**. They are suitable for logs,
//! metrics, and live status. A slow subscriber can miss events.
//!
//! A [`TaskWaiter`] uses a direct completion channel. Event-bus lag does not
//! affect it. It normally returns the final [`TaskOutcome`] after retries and
//! registry cleanup; it returns a runtime error if the completion channel closes
//! first. Create one with [`SupervisorHandle::add_and_watch`] or its fail-fast
//! `try_*` form. With the `controller` feature, use `submit_and_watch` or
//! `try_submit_and_watch`.
//!
//! ## Quick Start
//!
//! ```rust
//! use taskvisor::prelude::*;
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
//!
//!     let hello: TaskRef = TaskFn::arc("hello", |_ctx| async move {
//!         println!("hello from taskvisor");
//!         Ok(())
//!     });
//!
//!     supervisor.run(vec![TaskSpec::once(hello)]).await?;
//!     Ok(())
//! }
//! ```
//!
//! ## Main Types
//!
//! | Need | Types |
//! |------|-------|
//! | Define work | [`Task`], [`TaskFn`], [`TaskContext`], [`TaskSpec`] |
//! | Run work | [`Supervisor`], [`SupervisorHandle`] |
//! | Set defaults | [`SupervisorConfig`], [`TaskDefaults`] |
//! | Control retries | [`RestartPolicy`], [`BackoffPolicy`], [`JitterPolicy`] |
//! | Observe progress | [`Event`], [`EventKind`], [`Subscribe`] |
//! | Wait for the end | [`TaskWaiter`], [`TaskOutcome`] |
//! | Handle errors | [`Error`], [`TaskError`], [`RuntimeError`] |
//!
//! Main types are re-exported at the crate root. The module pages explain each
//! area in more detail: [`tasks`], [`policies`], [`events`], [`subscribers`],
//! [`core`], and [`identity`].
//!
//! ## Optional Features
//!
//! - `controller`: slot-based admission with queue, replace, and reject rules.
//! - `tracing`: forwards lifecycle events to `tracing`.
//! - `logging`: simple event logging for examples and development.
//! - `tokio-util-interop`: exposes the underlying Tokio cancellation token.
//! - `test-util`: constructors for task contexts, identities, and outcomes in tests.
//!
//! ## Examples
//!
//! Repository examples go from simple to advanced
//! ([browse them on GitHub](https://github.com/soltiHQ/taskvisor/tree/main/examples)):
//!
//! | Example | What it shows |
//! |---------------------------------------------------------------------------------------------|-------------------------------------------------------------------|
//! | [basic](https://github.com/soltiHQ/taskvisor/blob/main/examples/basic.rs)                   | Run one task and exit — the minimal wiring                        |
//! | [worker](https://github.com/soltiHQ/taskvisor/blob/main/examples/worker.rs)                 | A long-running worker that stops cleanly on Ctrl+C                |
//! | [periodic](https://github.com/soltiHQ/taskvisor/blob/main/examples/periodic.rs)             | Repeat a job after each successful cycle                          |
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
    ConfigError, Supervisor, SupervisorBuilder, SupervisorConfig, SupervisorHandle, TaskDefaults,
    TaskOutcome, TaskWaiter,
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
