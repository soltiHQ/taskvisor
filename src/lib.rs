//! # Taskvisor
//!
//! Taskvisor supervises in-process Tokio tasks that need retries, cancellation, final outcomes,
//! or coordinated shutdown. Its optional controller resolves competing work independently per
//! application key: queue it, replace older work, or reject it.
//!
//! ## Check the fit
//!
//! Taskvisor is useful when an application needs one or more of these:
//!
//! - tasks are added, removed, or watched while the service is running;
//! - task attempts need timeouts, retry limits, or backoff;
//! - application logic needs the final outcome of one submitted task;
//! - competing work for the same key must queue, replace older work, or be rejected.
//!
//! Taskvisor is not a persistent job queue. Runtime state, queued submissions, and task IDs
//! do not survive process exit. Use durable external storage when work must resume after a restart.
//!
//! ## Quick start
//!
//! A [`TaskFn`] turns an async closure into supervised work.
//! A [`TaskSpec`] gives that work a name and selects its lifecycle.
//!
//! ```rust
//! use taskvisor::prelude::*;
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
//!     let hello = TaskFn::arc(|_ctx| async {
//!         println!("hello from Taskvisor");
//!         Ok(())
//!     });
//!
//!     supervisor
//!         .run(vec![TaskSpec::once("hello", hello)])
//!         .await?;
//!     Ok(())
//! }
//! ```
//!
//! [`Supervisor::run`] accepts the complete static batch or rejects it. The method returns after
//! the shared cleanup workflow, not with each task's outcome.
//! Use a watched dynamic add when application logic needs that result.
//!
//! ## Continue with a runnable example
//!
//! The [examples guide] includes the learning path, commands, feature flags, and stop behavior.
//!
//! - Foundations: [basic], [task type], [graceful worker], [application shutdown], [periodic],
//!   [restart policies], and [configuration].
//! - Runtime patterns: [outcomes], [dynamic tasks], [queue consumer], and [CPU job].
//! - Observability: [custom subscriber], [logging], [tracing], and [metrics].
//! - Keyed admission: [controller slots], [controller admission], and [tenant sync].
//!
//! [examples guide]: https://github.com/soltiHQ/taskvisor/blob/main/examples/README.md
//! [basic]: https://github.com/soltiHQ/taskvisor/blob/main/examples/basic.rs
//! [task type]: https://github.com/soltiHQ/taskvisor/blob/main/examples/task_type.rs
//! [graceful worker]: https://github.com/soltiHQ/taskvisor/blob/main/examples/graceful_worker.rs
//! [application shutdown]: https://github.com/soltiHQ/taskvisor/blob/main/examples/application_shutdown.rs
//! [periodic]: https://github.com/soltiHQ/taskvisor/blob/main/examples/periodic.rs
//! [restart policies]: https://github.com/soltiHQ/taskvisor/blob/main/examples/restart_policies.rs
//! [configuration]: https://github.com/soltiHQ/taskvisor/blob/main/examples/configuration.rs
//! [outcomes]: https://github.com/soltiHQ/taskvisor/blob/main/examples/outcomes.rs
//! [dynamic tasks]: https://github.com/soltiHQ/taskvisor/blob/main/examples/dynamic_tasks.rs
//! [queue consumer]: https://github.com/soltiHQ/taskvisor/blob/main/examples/queue_consumer.rs
//! [CPU job]: https://github.com/soltiHQ/taskvisor/blob/main/examples/cpu_job.rs
//! [custom subscriber]: https://github.com/soltiHQ/taskvisor/blob/main/examples/custom_subscriber.rs
//! [logging]: https://github.com/soltiHQ/taskvisor/blob/main/examples/logging.rs
//! [tracing]: https://github.com/soltiHQ/taskvisor/blob/main/examples/tracing.rs
//! [metrics]: https://github.com/soltiHQ/taskvisor/blob/main/examples/metrics.rs
//! [tenant sync]: https://github.com/soltiHQ/taskvisor/blob/main/examples/tenant_sync.rs
//! [controller slots]: https://github.com/soltiHQ/taskvisor/blob/main/examples/controller_slots.rs
//! [controller admission]: https://github.com/soltiHQ/taskvisor/blob/main/examples/controller_admission.rs
//!
//! ## Choose the runtime entry point
//!
//! | Entry point                         | Use it when                                  |
//! |-------------------------------------|----------------------------------------------|
//! | [`Supervisor::run`]                 | A fixed batch finishes naturally             |
//! | [`Supervisor::run_until`]           | A fixed batch stops on an application future |
//! | [`Supervisor::run_with_os_signals`] | Taskvisor should install signal handlers     |
//! | [`Supervisor::serve`]               | Work is added and managed at runtime         |
//!
//! `run` and `run_until` do not install operating-system signal handlers.
//! `run_with_os_signals` is the explicit process-wide opt-in. Dynamic mode
//! returns a [`SupervisorHandle`] with add, query, cancel, remove, and shutdown methods.
//!
//! [`Supervisor::new`] accepts runtime configuration and subscribers with default task settings.
//! Use [`Supervisor::builder`] when you need custom [`TaskDefaults`], controller admission,
//! or typed construction errors through [`SupervisorBuilder::try_build`].
//!
//! ## Choose task behavior
//!
//! | Constructor               | After success                  | After a retry-eligible failure |
//! |---------------------------|--------------------------------|--------------------------------|
//! | [`TaskSpec::once`]        | Stop                           | Stop                           |
//! | [`TaskSpec::restartable`] | Stop                           | Retry if the limit allows      |
//! | [`TaskSpec::periodic`]    | Wait its interval, then repeat | Retry if the limit allows      |
//!
//! Each registration has one [`TaskId`] and one internal actor. Attempts for that ID never overlap.
//! [`RestartPolicy`] decides whether success repeats and whether a retryable failure may run again.
//! The retry limit restricts only repeats after failure. [`BackoffPolicy`] and [`JitterPolicy`]
//! control failure delays. A timeout applies to one attempt. The default retry limit is unlimited;
//! set [`TaskSpec::with_max_retries`] or a [`TaskDefaults`] limit when repeated failure must eventually stop the task.
//!
//! [`Task::spawn`] should return its future promptly. Put the task's work inside that future, and move
//! blocking or CPU-heavy work off Tokio worker threads. Long-running work must observe [`TaskContext::cancelled`]
//! or use [`TaskContext::run_until_cancelled`]. Return [`TaskError::Canceled`] after a cooperative stop.
//! Return [`TaskError::Fail`] for a retry-eligible failure or [`TaskError::Fatal`] when the actor must stop.
//!
//! ## Get results or observe events
//!
//! [`SupervisorHandle::add_and_watch`] returns a [`TaskWaiter`] for a direct final [`TaskOutcome`].
//! Controller users can choose [`SupervisorHandle::submit_and_watch`]. A watched result does not
//! depend on the lossy event path, but it is still in-memory and is not durable across process termination.
//!
//! [`Event`] and [`Subscribe`] are for logs, metrics, tracing, and live diagnostics. The shared event bus
//! and each subscriber queue are bounded. Event delivery is best-effort and must not drive application correctness.
//!
//! ## Coordinate work by key
//!
//! The default `controller` feature adds keyed admission before registry entry.
//! Enable the controller for a supervisor with [`SupervisorBuilder::with_controller`], then submit a [`ControllerSpec`].
//!
//! ```text
//! ControllerSpec ──► controller slot
//!                         ├── idle ──► registry admission
//!                         └── busy ──► queue, replace, or reject
//! ```
//!
//! A task name is the registry uniqueness key. A controller slot is the key used to coordinate competing submissions.
//! Different task names can share a slot. Direct `add*` methods bypass this layer; `submit*` methods use it.
//! See [`AdmissionPolicy`] for the exact queue, replace, and reject behavior.
//!
//! ## Cancellation and shutdown boundary
//!
//! Cancellation starts cooperatively. At the configured grace deadline, Taskvisor may report [`TaskOutcome::ForceAborted`]
//! while it keeps owning the unfinished actor until physical exit. While that actor remains active, its synchronous
//! task code or attempt-future destructor may keep its task name and capacity reservation owned.
//! Later isolated destruction of terminal task values keeps capacity reserved but does not keep the task name reserved.
//!
//! Dropping a non-final public owner leaves the runtime running. Dropping the final owner can request cancellation but
//! cannot wait for cleanup. Call [`SupervisorHandle::shutdown`] when the cleanup result matters.
//!
//! ## Architecture at a glance
//!
//! ```text
//! application
//!      ├── static batch ──► Supervisor::run*
//!      ├── dynamic task ──► SupervisorHandle::add*
//!      └── keyed task ──► SupervisorHandle::submit* ──► controller
//!
//! registry ──► TaskActor ──► sequential attempts
//!
//! runtime components ──► bounded event bus ──► subscriber queues
//!
//! registry cleanup or watched rejection ──► TaskWaiter
//! ```
//!
//! The registry is the source of truth for registered task membership. The controller owns
//! submissions that have not reached the registry. Events only observe the lifecycle.
//! Watched outcomes use a separate one-shot path.
//!
//! ## Crate layout
//!
//! - [`tasks`] defines work, cancellation context, and task specifications.
//! - [`policies`] defines restart and retry timing.
//! - [`core`] exposes construction, runtime control, outcomes, and configuration.
//! - [`controller`] defines optional keyed admission.
//! - [`events`] and [`subscribers`] define best-effort observability.
//! - [`error`] maps the public error types to their API boundaries.
//! - [`identity`] explains task IDs, names, and controller slots.
//! - [`prelude`] re-exports the common application-facing types.
//!
//! Contributors can follow the [source guide](https://github.com/soltiHQ/taskvisor/blob/main/src/ARCHITECTURE.md)
//! for runtime ownership, data flow, and test entry points.
//!
//! ## Feature flags
//!
//! - `controller` enables keyed admission and is enabled by default.
//! - `logging` enables the built-in standard-output subscriber.
//! - `tracing` enables the built-in `tracing` bridge.
//! - `tokio-util-interop` exposes Tokio's cancellation token type.
//! - `test-util` exposes constructors intended for external tests.

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
pub use error::{BoxError, BuildError, Error, RuntimeError, SharedError, TaskError};

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
    PreparedSubmission, SlotStatusKind, SlotView,
};

#[cfg(feature = "logging")]
#[cfg_attr(docsrs, doc(cfg(feature = "logging")))]
pub use subscribers::LogWriter;

#[cfg(feature = "tracing")]
#[cfg_attr(docsrs, doc(cfg(feature = "tracing")))]
pub use subscribers::{TracingBridge, TracingBridgeWithReasons};
