//! Coordinates tasks that target the same application resource.
//!
//! The controller is an optional admission layer in front of the runtime registry.
//! It gives each application-defined **slot** at most one owner.
//! Work in different slots can proceed independently.
//!
//! Use [`SupervisorHandle::submit`](crate::SupervisorHandle::submit) when tasks for the same
//! customer, device, document, deployment, or other key must not overlap.
//! Use [`SupervisorHandle::add`](crate::SupervisorHandle::add) when keyed admission is not needed;
//! direct adds bypass this module.
//!
//! The `controller` crate feature is enabled by default. A supervisor still needs an explicit
//! [`SupervisorBuilder::with_controller`](crate::SupervisorBuilder::with_controller)
//! call before controller methods can accept work.
//!
//! # Quick start
//!
//! This example submits a job to a customer-specific lane and receives its final result through a dedicated waiter:
//!
//! ```rust,no_run
//! use taskvisor::prelude::*;
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let supervisor = Supervisor::builder(SupervisorConfig::default())
//!     .with_controller(ControllerConfig::default())
//!     .build();
//! let handle = supervisor.serve()?;
//!
//! let task = TaskFn::arc(|_ctx| async { Ok(()) });
//! let request = ControllerSpec::queue(TaskSpec::once("customer-42-job-7", task))
//!     .with_slot("customer-42");
//!
//! let waiter = handle.submit(request).watch().execute().await?;
//! println!("{:?}", waiter.wait().await?);
//! handle.shutdown().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Architecture
//!
//! ```text
//! application
//!      │ ControllerSpec
//!      ▼
//! SupervisorHandle::submit
//!      │ command intake
//!      ▼
//! controller slot
//!      ├── idle ──► runtime registry ──► managed task
//!      └── busy ──► queue, replace, or reject
//! ```
//!
//! A slot stays occupied while its owner is being admitted, registered, or physically released.
//! "One owner" does not mean that a task body is polling at every moment.
//! Runtime-wide admission limits still apply after the controller selects work from a slot.
//!
//! # Slot, task name, and task ID
//!
//! These values have separate roles:
//!
//! - a **slot** groups work that must not overlap;
//! - a [`TaskSpec`](crate::TaskSpec) name is a unique registry key and label;
//! - a [`TaskId`](crate::TaskId) is the identity of one submission and outcome.
//!
//! The task name is the default slot. Use [`ControllerSpec::with_slot`] to put differently named
//! tasks in one admission lane. Slot admission does not reserve a task name.
//! The runtime registry still checks name uniqueness.
//!
//! Cancellation and removal never act on an entire slot.
//! `TaskId` targets can claim queued or registered work.
//! Name targets passed to [`SupervisorHandle::remove`](crate::SupervisorHandle::remove)
//! or [`SupervisorHandle::cancel`](crate::SupervisorHandle::cancel) see only work already in the
//! registry because queued submissions do not own a registered name.
//! Removing one queued item leaves the other submissions in its slot unchanged.
//!
//! # Choose a busy-slot policy
//!
//! - [`AdmissionPolicy::Queue`] appends to a bounded FIFO queue. Use it when every item should be considered in order.
//! - [`AdmissionPolicy::Replace`] retires the owner and replaces the queue head. Use it when the next item should carry the newest value.
//! - [`AdmissionPolicy::DropIfRunning`] rejects the new item without running it. Use it when duplicate work can be skipped.
//!
//! After preflight, every policy takes the same idle-slot path and attempts registry admission.
//! `Replace` changes only the queue head; older FIFO entries behind it remain.
//!
//! # Configure a submission
//!
//! [`SupervisorHandle::submit`](crate::SupervisorHandle::submit) creates a [`Submit`] operation.
//!
//! - `execute().await` waits for ownership and command capacity, then returns the task ID.
//! - `ownership_timeout(duration)` bounds only ownership admission before `execute().await`.
//! - `try_intake()` requires ownership and command capacity to be available immediately.
//! - `watch()` changes a successful terminal result from [`TaskId`](crate::TaskId) to [`TaskWaiter`](crate::TaskWaiter), including for ownership-bounded and fail-fast intake.
//! - Allocate the `TaskId` before intake or events with [`SupervisorHandle::prepare_submission`](crate::SupervisorHandle::prepare_submission).
//!
//! `Ok(id)` from an unwatched terminal confirms only command intake. Slot admission and runtime registration happen later.
//! Add `watch()` when application logic must know whether work was rejected or how an admitted task ended.
//! [`TaskWaiter`](crate::TaskWaiter) delivers that result directly; lifecycle events remain a best-effort observability path.
//! `ownership_timeout` stops its timer after the permit is acquired. It does not bound controller-command capacity,
//! slot admission, later registry admission, or task execution. On timeout, no command or lifecycle event is produced.
//! [`PreparedSubmission::submit`] creates the same operation while preserving its preallocated ID.
//!
//! During shutdown, buffered and controller-owned pending submissions are rejected.
//! A watched pending submission reports [`RejectionKind::ControllerShuttingDown`](crate::RejectionKind::ControllerShuttingDown).
//! Work already accepted by the runtime follows the normal runtime shutdown process.
//!
//! # Operations
//!
//! - [`ControllerSpec`] combines a task, slot, and admission policy.
//! - [`PreparedSubmission`] exposes an allocated `TaskId` before intake.
//! - [`ControllerConfig`] bounds intake, slots, pending work, and operations.
//! - [`ControllerSnapshot`] provides a rolling operational view of slot state.
//! - [`ControllerError`] reports failures before command intake completes.

mod snapshot;
pub use snapshot::{ControllerSnapshot, SlotStatusKind, SlotView};

mod policy;
pub use policy::AdmissionPolicy;

mod config;
pub use config::ControllerConfig;

mod engine;
pub(crate) use engine::Controller;

mod error;
pub use error::ControllerError;

mod prepared;
pub use prepared::{PreparedSubmission, Submit};

mod spec;
pub use spec::ControllerSpec;
