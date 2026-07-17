//! # taskvisor
//!
//! Taskvisor supervises Tokio tasks with restart/backoff, reliable outcomes, and keyed queue/replace/drop admission.
//! It can time out attempts, stop tasks during shutdown, and dynamically manage work.
//!
//! Use it for workers, consumers, connection loops, and other background work that should have a clear lifecycle.
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
//!          ▼
//!       TaskSpec  ── fills missing values from ──► TaskDefaults
//!          ▼
//!      Supervisor
//!       │       │
//!       │       └── best-effort events ──► Subscribe
//!       │
//!       └── watched final result ────────► TaskWaiter
//! ```
//!
//! ## Choose a Mode
//!
//! - **Static:** call [`Supervisor::run`] with a known task set.
//!   It waits for all tasks to finish or for an OS shutdown signal.
//! - **Dynamic:** call [`Supervisor::serve`].
//!   It returns a [`SupervisorHandle`] that can add, remove, cancel, and list tasks while the service is running.
//!
//! `run` registers its initial task list as one batch.
//! If a name is repeated or already in use, no task from that batch starts.
//!
//! ## One Task, Many Attempts
//!
//! A registered task has one [`TaskId`], but it may run several attempts:
//!
//! ```text
//! register
//!    ▼
//! attempt 1 ── failure ──► backoff ──► attempt 2 ── success ──► finish
//!                                                       ▼
//!                                                 TaskOutcome
//! ```
//!
//! Attempts for one task never overlap.
//! [`RestartPolicy`] decides whether a new attempt is allowed.
//! [`BackoffPolicy`] sets the delay after a retryable failure.
//! A timeout applies to one attempt, not to the full task lifetime.
//!
//! ## Cancellation and Shutdown
//!
//! Cancellation is cooperative first.
//! A long-running task should await [`TaskContext::cancelled`] or use [`TaskContext::run_until_cancelled`], then return [`TaskError::Canceled`].
//! During shutdown, taskvisor waits for the configured grace period.
//! It aborts tasks that still have not stopped.
//!
//! Dropping one supervisor or handle clone does not stop the runtime.
//! Dropping the last public owner sends best-effort cancellation, but cannot wait for cleanup.
//! Call [`SupervisorHandle::shutdown`] when cleanup must be complete before your code continues.
//!
//! ## Events or Final Outcomes?
//!
//! Lifecycle [`Event`] values are **best-effort**.
//! They are suitable for logs, metrics, and live status.
//! A slow subscriber can miss events.
//!
//! A [`TaskWaiter`] uses a direct completion channel.
//! Event-bus lag does not affect it.
//!
//! It normally returns the final [`TaskOutcome`] after retries and registry cleanup;
//! it returns a runtime error if the completion channel closes first.
//!
//! Create one with [`SupervisorHandle::add_and_watch`] or its fail-fast `try_*` form.
//! With the `controller` feature, use `submit_and_watch` or `try_submit_and_watch`.
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
//! | Need             | Types                                                  |
//! |------------------|--------------------------------------------------------|
//! | Define work      | [`Task`], [`TaskFn`], [`TaskContext`], [`TaskSpec`]    |
//! | Run work         | [`Supervisor`], [`SupervisorHandle`]                   |
//! | Set defaults     | [`SupervisorConfig`], [`TaskDefaults`]                 |
//! | Control retries  | [`RestartPolicy`], [`BackoffPolicy`], [`JitterPolicy`] |
//! | Observe progress | [`Event`], [`EventKind`], [`Subscribe`]                |
//! | Wait for the end | [`TaskWaiter`], [`TaskOutcome`], [`TaskOutcomeKind`]   |
//! | Handle errors    | [`Error`], [`TaskError`], [`RuntimeError`]             |
//!
//! Main types are re-exported at the crate root.
//! The module pages explain each area in more detail:
//! [`tasks`], [`policies`], [`events`], [`subscribers`], [`core`], and [`identity`].
//!
//! ## Optional Features
//!
//! - `tracing`: forwards lifecycle events to `tracing`.
//! - `logging`: simple event logging for examples and development.
//! - `tokio-util-interop`: exposes the underlying Tokio cancellation token.
//! - `controller` (default): slot-based admission with queue, replace, and reject rules.
//! - `test-util`: constructors for task contexts, identities, and outcomes in tests.
//!
//! ## Examples
//!
//! Follow the groups in order, or [browse all examples on GitHub](https://github.com/soltiHQ/taskvisor/tree/main/examples).
//!
//! ### Start here
//!
//! | Example                                                                                   | What it shows                                                   |
//! |-------------------------------------------------------------------------------------------|-----------------------------------------------------------------|
//! | [basic](https://github.com/soltiHQ/taskvisor/blob/main/examples/basic.rs)                 | Run one task and exit — the minimal wiring                      |
//! | [worker](https://github.com/soltiHQ/taskvisor/blob/main/examples/worker.rs)               | A long-running worker that stops cleanly on Ctrl+C              |
//! | [periodic](https://github.com/soltiHQ/taskvisor/blob/main/examples/periodic.rs)           | Repeat a job after each successful cycle                        |
//! | [multiple](https://github.com/soltiHQ/taskvisor/blob/main/examples/multiple.rs)           | Several restart rules under one supervisor                      |
//!
//! ### Real patterns
//!
//! | Example                                                                                     | What it shows                                                   |
//! |---------------------------------------------------------------------------------------------|-----------------------------------------------------------------|
//! | [queue_consumer](https://github.com/soltiHQ/taskvisor/blob/main/examples/queue_consumer.rs) | A message consumer that reconnects after failures               |
//! | [cpu_job](https://github.com/soltiHQ/taskvisor/blob/main/examples/cpu_job.rs)               | Run CPU-heavy work on rayon without blocking Tokio              |
//!
//! ### Observability
//!
//! | Example                                                                                 | What it shows                                             |
//! |-----------------------------------------------------------------------------------------|-----------------------------------------------------------|
//! | [subscriber](https://github.com/soltiHQ/taskvisor/blob/main/examples/subscriber.rs)     | React to lifecycle events with your own handler           |
//! | [tracing](https://github.com/soltiHQ/taskvisor/blob/main/examples/tracing.rs)           | Send events into `tracing` (feature `tracing`)            |
//! | [metrics](https://github.com/soltiHQ/taskvisor/blob/main/examples/metrics.rs)           | Build Prometheus counters from lifecycle events           |
//!
//! ### Dynamic work and outcomes
//!
//! | Example                                                                                 | What it shows                                            |
//! |-----------------------------------------------------------------------------------------|----------------------------------------------------------|
//! | [dynamic](https://github.com/soltiHQ/taskvisor/blob/main/examples/dynamic.rs)           | Add, list, cancel, and remove tasks at runtime           |
//! | [outcomes](https://github.com/soltiHQ/taskvisor/blob/main/examples/outcomes.rs)         | Wait for a task's reliable final result                  |
//!
//! ### Keyed admission
//!
//! | Example                                                                                       | What it shows                                         |
//! |-----------------------------------------------------------------------------------------------|-------------------------------------------------------|
//! | [tenant_sync](https://github.com/soltiHQ/taskvisor/blob/main/examples/tenant_sync.rs)         | Keep only the latest sync revision per tenant         |
//! | [slots](https://github.com/soltiHQ/taskvisor/blob/main/examples/slots.rs)                     | Compare queue, replace, and drop policies             |
//! | [admission](https://github.com/soltiHQ/taskvisor/blob/main/examples/admission.rs)             | Observe typed admission and rejection outcomes       |

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

/// Compiles runnable Rust code blocks in `README.md` as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

pub mod core;
pub use core::{
    ConfigError, Supervisor, SupervisorBuilder, SupervisorConfig, SupervisorHandle, TaskDefaults,
    TaskOutcome, TaskOutcomeKind, TaskWaiter,
};

pub mod tasks;
pub use tasks::{BoxTaskFuture, Task, TaskContext, TaskFn, TaskRef, TaskSetting, TaskSpec};

pub mod policies;
pub use policies::{BackoffError, BackoffPolicy, JitterPolicy, RestartPolicy};

pub mod error;
pub use error::{BoxError, Error, RuntimeError, SharedError, TaskError};

pub mod events;
pub use events::{BackoffSource, Event, EventKind, RejectionKind};

pub mod subscribers;
pub use subscribers::Subscribe;

pub mod identity;
pub use identity::TaskId;

pub mod prelude;

pub(crate) mod reasons;

#[cfg(feature = "controller")]
#[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
pub mod controller;
#[cfg(feature = "controller")]
#[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
pub use controller::{
    AdmissionPolicy, ControllerConfig, ControllerError, ControllerSnapshot, ControllerSpec,
    SlotStatusKind, SlotView,
};

#[cfg(feature = "logging")]
#[cfg_attr(docsrs, doc(cfg(feature = "logging")))]
pub use subscribers::LogWriter;

#[cfg(feature = "tracing")]
#[cfg_attr(docsrs, doc(cfg(feature = "tracing")))]
pub use subscribers::TracingBridge;
