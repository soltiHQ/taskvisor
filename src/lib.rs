//! # Taskvisor
//!
//! Taskvisor supervises in-process Tokio tasks that need retries, cancellation, final outcomes, or coordinated shutdown.
//! Its optional controller queues, replaces, or rejects competing work by application key.
//! Supervisor-wide limits still apply.
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
//! Taskvisor is not a persistent job queue.
//! Runtime state, queued submissions, and task IDs do not survive process exit.
//! Use durable external storage when work must resume after a restart.
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
//! [`Supervisor::run`] accepts the complete static batch or rejects it.
//! The method returns after the shared cleanup workflow, not with each task's outcome.
//! Use a watched dynamic add when application logic needs that result.
//!
//! ## Continue with a runnable example
//!
//! The [user guide] explains the application workflow from task definition through production boundaries.
//! The [examples guide] lists complete programs, commands, feature flags, and shutdown behavior.
//!
//! [user guide]: https://github.com/soltiHQ/taskvisor/blob/main/docs/index.md
//! [examples guide]: https://github.com/soltiHQ/taskvisor/blob/main/examples/README.md
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
//! `run_with_os_signals` is the explicit process-wide opt-in.
//! Dynamic mode returns a [`SupervisorHandle`] for runtime management and shutdown.
//!
//! [`Supervisor::new`] accepts runtime configuration and subscribers with default task settings.
//! [`Supervisor::builder`] supports custom [`TaskDefaults`] and controller admission.
//! [`SupervisorBuilder::try_build`] reports typed construction errors.
//!
//! ## Choose task behavior
//!
//! | Constructor               | After success                           | After a retry-eligible failure |
//! |---------------------------|-----------------------------------------|--------------------------------|
//! | [`TaskSpec::once`]        | Stop                                    | Stop                           |
//! | [`TaskSpec::restartable`] | Stop                                    | Retry if the limit allows      |
//! | [`TaskSpec::periodic`]    | Wait at least its interval, then repeat | Retry if the limit allows      |
//!
//! Each registration has one [`TaskId`].
//! Attempts for the same ID never overlap.
//! [`RestartPolicy`] decides whether success repeats and whether a retryable failure may run again.
//! The retry limit restricts only repeats after failure.
//! [`BackoffPolicy`] and [`JitterPolicy`] control failure delays.
//! A timeout applies to one attempt.
//! The default retry limit is unlimited.
//! Set [`TaskSpec::with_max_retries`] or a [`TaskDefaults`] limit when repeated failure must eventually stop the task.
//!
//! [`Task::spawn`] should return its future promptly.
//! Put the task's work inside that future.
//! Move blocking or CPU-heavy work off Tokio worker threads.
//! Long-running work must observe [`TaskContext::cancelled`] or use [`TaskContext::run_until_cancelled`].
//! Return [`TaskError::Canceled`] after a cooperative stop.
//! Return [`TaskError::Fail`] for a retry-eligible failure or [`TaskError::Fatal`] when the actor must stop.
//!
//! ## Get results or observe events
//!
//! Executing a watched [`SupervisorHandle::add`] returns a [`TaskWaiter`] for its final [`TaskOutcome`].
//! A watched result does not depend on the lossy event path.
//! It remains in-memory and is not durable across process termination.
#![cfg_attr(
    feature = "controller",
    doc = "Controller users can add `watch()` to the operation returned by [`SupervisorHandle::submit`]."
)]
//!
//! [`Event`] and [`Subscribe`] are for logs, metrics, tracing, and live diagnostics.
//! The shared event bus and each subscriber queue are bounded.
//! Event delivery is best-effort and must not drive application correctness.
#![cfg_attr(
    feature = "controller",
    doc = r#"
## Coordinate work by key

The default `controller` feature adds keyed admission before registry entry.
Enable the controller for a supervisor with [`SupervisorBuilder::with_controller`], then submit a [`ControllerSpec`].

```text
ControllerSpec ──► controller slot
                        ├── idle ──► registry admission
                        └── busy ──► queue, replace, or reject
```

A task name is the registry uniqueness key inside one supervisor.
A controller slot coordinates submissions that must not run together.
Different task names can share a slot.
[`SupervisorHandle::add`] bypasses keyed admission.
[`SupervisorHandle::submit`] uses it.
See [`AdmissionPolicy`] for the exact queue, replace, and reject behavior.
"#
)]
//!
//! ## Cancellation and shutdown boundary
//!
//! Cancellation starts cooperatively.
//! After grace expires, [`TaskOutcome::ForceAborted`] can arrive before the actor exits physically.
//! Taskvisor keeps owning that actor until physical exit.
//! While it remains active, synchronous code or an attempt-future destructor can keep its task name and capacity reservation owned.
//! Later isolated destruction of terminal task values keeps capacity reserved but does not keep the task name reserved.
//! Use [`Supervisor::ownership_snapshot`] or [`SupervisorHandle::ownership_snapshot`] to inspect that separate boundary.
//!
//! Dropping a non-final public owner leaves the runtime running.
//! Dropping the final owner can request cancellation but cannot wait for cleanup.
//! Call [`SupervisorHandle::shutdown`] when the cleanup result matters.
//!
//! ## Architecture at a glance
//!
//! ```text
//! application
//!      ├── static batch ──► Supervisor::run*
//!      ├── dynamic task ──► SupervisorHandle::add
//!      └── keyed task ──► SupervisorHandle::submit ──► controller
//!
//! registry ──► TaskActor ──► sequential attempts
//!
//! runtime components ──► bounded event bus ──► subscriber queues
//!
//! registry cleanup or watched rejection ──► TaskWaiter
//! ```
//!
//! The registry is the source of truth for registered task membership.
//! The controller owns submissions that have not reached the registry.
//! Events only observe the lifecycle.
//! Watched outcomes use a separate one-shot path.
//!
//! ## Crate layout
//!
//! - [`tasks`] defines work, cancellation context, and task specifications.
//! - [`policies`] defines restart and retry timing.
//! - [`core`] exposes construction, runtime control, outcomes, and configuration.
#![cfg_attr(
    feature = "controller",
    doc = "- [`controller`] defines optional keyed admission."
)]
//! - [`events`] and [`subscribers`] define best-effort observability.
//! - [`error`] maps the public error types to their API boundaries.
//! - [`identity`] explains task IDs, names, and controller slots.
//! - [`prelude`] re-exports the common application-facing types.
//!
//! The [source guide](https://github.com/soltiHQ/taskvisor/blob/main/src/ARCHITECTURE.md) maps runtime ownership, data flow, and test entry points.
//!
//! ## Feature flags
//!
//! - `controller` enables keyed admission and is enabled by default.
//! - `logging` enables the built-in standard-output subscriber.
//! - `tracing` enables the built-in `tracing` bridge.
//! - `tokio-util-interop` exposes Tokio's cancellation token type.
//! - `test-util` exposes constructors intended for external tests.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations, missing_docs, unreachable_pub)]
#![cfg_attr(docsrs, feature(doc_cfg))]

/// Compiles runnable Rust code blocks in `README.md` when its controller API is available.
#[cfg(all(doctest, feature = "controller"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

/// Compiles runnable Rust code blocks in the guide index as doctests.
#[cfg(doctest)]
#[doc = include_str!("../docs/index.md")]
struct GuideIndexDoctests;

/// Compiles runnable Rust code blocks in the quick-start guide as doctests.
#[cfg(doctest)]
#[doc = include_str!("../docs/quick-start.md")]
struct QuickStartGuideDoctests;

/// Compiles runnable Rust code blocks in the mental-model guide as doctests.
#[cfg(doctest)]
#[doc = include_str!("../docs/mental-model.md")]
struct MentalModelGuideDoctests;

/// Compiles runnable Rust code blocks in the installation guide as doctests.
#[cfg(doctest)]
#[doc = include_str!("../docs/installation.md")]
struct InstallationGuideDoctests;

/// Compiles runnable Rust code blocks in the task-definition guide as doctests.
#[cfg(doctest)]
#[doc = include_str!("../docs/defining-tasks.md")]
struct DefiningTasksGuideDoctests;

/// Compiles runnable Rust code blocks in the lifecycle-policy guide as doctests.
#[cfg(doctest)]
#[doc = include_str!("../docs/lifecycle-policies.md")]
struct LifecyclePoliciesGuideDoctests;

/// Compiles runnable Rust code blocks in the supervisor entry-point guide as doctests.
#[cfg(doctest)]
#[doc = include_str!("../docs/running-and-managing.md")]
struct RunTaskvisorGuideDoctests;

/// Compiles runnable Rust code blocks in the dynamic task-management guide as doctests.
#[cfg(doctest)]
#[doc = include_str!("../docs/managing-tasks.md")]
struct ManagingTasksGuideDoctests;

/// Compiles runnable Rust code blocks in the cancellation guide as doctests.
#[cfg(doctest)]
#[doc = include_str!("../docs/cancellation-and-shutdown.md")]
struct CancellationAndShutdownGuideDoctests;

/// Compiles runnable Rust code blocks in the outcome and event guide as doctests.
#[cfg(doctest)]
#[doc = include_str!("../docs/outcomes-and-events.md")]
struct OutcomesAndEventsGuideDoctests;

/// Compiles runnable Rust code blocks in the keyed-admission guide as doctests.
#[cfg(all(doctest, feature = "controller"))]
#[doc = include_str!("../docs/keyed-admission.md")]
struct KeyedAdmissionGuideDoctests;

/// Compiles runnable Rust code blocks in the configuration guide as doctests.
#[cfg(doctest)]
#[doc = include_str!("../docs/configuration.md")]
struct ConfigurationGuideDoctests;

/// Compiles runnable Rust code blocks in the production-boundaries guide as doctests.
#[cfg(doctest)]
#[doc = include_str!("../docs/production-boundaries.md")]
struct ProductionBoundariesGuideDoctests;

/// Compiles runnable Rust code blocks in the common-mistakes guide as doctests.
#[cfg(doctest)]
#[doc = include_str!("../docs/common-mistakes.md")]
struct CommonMistakesGuideDoctests;

pub mod core;
pub use core::{
    AddOperation, CancelOperation, ConfigError, OwnershipSnapshot, RemoveOperation, Supervisor,
    SupervisorBuilder, SupervisorConfig, SupervisorHandle, TaskDefaults, TaskOutcome,
    TaskOutcomeKind, TaskTarget, TaskWaiter,
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
pub use subscribers::{Subscribe, SubscriberExecution};

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
    PreparedSubmission, SlotStatusKind, SlotView, Submit,
};

#[cfg(feature = "logging")]
#[cfg_attr(docsrs, doc(cfg(feature = "logging")))]
pub use subscribers::LogWriter;

#[cfg(feature = "tracing")]
#[cfg_attr(docsrs, doc(cfg(feature = "tracing")))]
pub use subscribers::{TracingBridge, TracingBridgeWithReasons};
