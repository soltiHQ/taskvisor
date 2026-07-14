//! # Slot-based admission control
//!
//! The controller is an optional layer before the supervisor registry.
//! It decides when a submitted task may be registered.
//!
//! Use direct `add*` methods when you do not need this admission step.
//! This module requires the `controller` feature.
//!
//! Use the controller when work with the same key must not overlap:
//! - one deployment per environment;
//! - one job per customer or resource;
//! - one rebuild per index;
//! - skip or replace duplicate work while a lane is busy.
//!
//! ## Slots and task names
//!
//! A **slot** is one sequential admission lane. The controller admits by slot, not by task name.
//!
//! A slot is the key returned by [`ControllerSpec::slot_name`].
//! If no slot is set, it defaults to the task name.
//! Use [`ControllerSpec::with_slot`] to group several different task names into one slot.
//!
//! Task names belong to the runtime registry. They must be unique across all
//! currently registered tasks, even when those tasks use different slots.
//!
//! ```text
//! task "deploy-main-42" ┐
//! task "deploy-main-43" ├── slot "deploy-main"
//! task "deploy-main-44" ┘
//! ```
//!
//! These are different runtime tasks, but the controller admits them one at a
//! time through the same slot. Different slots may be occupied at the same
//! time. The supervisor's global concurrency limit still applies to attempts.
//!
//! ## Admission policies
//!
//! When a slot is idle, every policy tries to start admission.
//! When it is busy, the policy decides what happens to the new submission.
//!
//! | Policy                             | Busy slot behavior                                         |
//! |------------------------------------|------------------------------------------------------------|
//! | [`AdmissionPolicy::Queue`]         | enqueue if the per-slot limit allows it; otherwise reject  |
//! | [`AdmissionPolicy::Replace`]       | create or replace the queue head; retire owner if needed   |
//! | [`AdmissionPolicy::DropIfRunning`] | reject the submission                                      |
//!
//! `Replace` changes only the next-item position: it creates or replaces the queue head.
//! A displaced head is rejected; FIFO items behind it stay queued.
//! A watched rejected submission resolves to [`TaskOutcome::Rejected`](crate::TaskOutcome::Rejected).
//!
//! ## Slot states
//!
//! In this table, `Next / Idle` means: start admission for the next queued item, or become `Idle` when no queued item can start.
//!
//! | Current state | Cause                                                          | Next state                                           |
//! |---------------|----------------------------------------------------------------|------------------------------------------------------|
//! | `Idle`        | a submission starts admission                                  | `Admitting`                                          |
//! | `Admitting`   | registry accepts registration                                  | `Running`                                            |
//! | `Admitting`   | registry rejects registration                                  | Next / Idle                                          |
//! | `Admitting`   | `Replace` arrives                                              | `Terminating`                                        |
//! | `Running`     | `Replace` arrives                                              | `Terminating`                                        |
//! | `Running`     | terminal registry cleanup completes                            | Next / Idle                                          |
//! | `Terminating` | pending registration is accepted                               | stay `Terminating`; request owner removal            |
//! | `Terminating` | pending registration is rejected or terminal cleanup completes | Next / Idle                                          |
//! | `Terminating` | another `Replace` arrives                                      | stay `Terminating`; create or replace the queue head |
//!
//! Admitting + Replace is shown as `Terminating` in public snapshots.
//! Taskvisor first waits for the registration decision.
//! If registration succeeds, it then removes that owner before starting the replacement.
//!
//! Only `Replace` changes the public status to `Terminating`.
//! ID-based `remove*` and `cancel*` requests do not change the status by themselves.
//! The slot advances when the registry reports terminal cleanup.
//! `Queue` and `DropIfRunning` also leave the current owner's status unchanged.
//!
//! After a registered owner, the next queued task starts only after terminal registry cleanup.
//! At that point, the old task actor has been joined and its task name is free.
//! If registration is rejected, no registered owner exists; the controller can try the next queued item as soon as it receives that direct decision.
//! Events are only for observability; they do not drive slot state.
//!
//! ## Public API
//!
//! - Build submissions with [`ControllerSpec::queue`], [`ControllerSpec::replace`], or [`ControllerSpec::drop_if_running`].
//! - Configure with [`SupervisorBuilder::with_controller`](crate::SupervisorBuilder::with_controller).
//! - Submit with [`SupervisorHandle::submit`](crate::SupervisorHandle::submit) or [`SupervisorHandle::submit_and_watch`](crate::SupervisorHandle::submit_and_watch).
//!   Their `try_*` forms return immediately instead of waiting when the command channel is full.
//! - Remove or cancel by the [`TaskId`](crate::TaskId) returned from submission.
//! - Read current slot state with [`SupervisorHandle::controller_snapshot`](crate::SupervisorHandle::controller_snapshot).
//!
//! ## Example
//!
//! ```rust,no_run
//! use taskvisor::{
//!     ControllerConfig, ControllerSpec, Supervisor, SupervisorConfig, TaskFn, TaskSpec,
//! };
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let supervisor = Supervisor::builder(SupervisorConfig::default())
//!     .with_controller(ControllerConfig::default())
//!     .build();
//! let handle = supervisor.serve();
//!
//! let task = TaskFn::arc("refresh-customer-42", |_ctx| async { Ok(()) });
//! let request = ControllerSpec::queue(TaskSpec::once(task)).with_slot("customer-42");
//! let (_id, waiter) = handle.submit_and_watch(request).await?;
//!
//! assert!(waiter.wait().await?.is_success());
//! handle.shutdown().await?;
//! # Ok(())
//! # }
//! ```
//!
//! A successful controller submission call confirms only that the controller command queue accepted the submission.
//! Slot admission and registry registration happen later.
//! Use `submit_and_watch` or `try_submit_and_watch` when the final result matters.
//!
//! The returned `TaskId` is allocated before runtime admission.
//! If admission succeeds, the runtime uses the same ID.
//! Before admission, it still identifies queued work for cancellation, outcomes, and event correlation.
//!
//! [`ControllerConfig::queue_capacity`] bounds the controller command queue and separately caps registry-backed remove/cancel operations.
//! A new `Queue` submission is rejected when the slot's pending depth is already [`ControllerConfig::max_slot_queue`] or greater.
//! The current owner is not part of this depth, but a replacement head is.
//! `Replace` itself may create or replace that head even when the limit is zero.
//!
//! When the controller or registry rejects a watched submission before registration, its waiter resolves to [`TaskOutcome::Rejected`](crate::TaskOutcome::Rejected).
//! After registration, the waiter resolves to the runtime task's terminal outcome.
//!
//! ## Events
//!
//! Controller-specific event kinds are:
//! - [`EventKind::ControllerSlotTransition`](crate::EventKind::ControllerSlotTransition)
//! - [`EventKind::ControllerSubmitted`](crate::EventKind::ControllerSubmitted)
//! - [`EventKind::ControllerRejected`](crate::EventKind::ControllerRejected)
//!
//! Removing a submission that is still in a slot queue also publishes the standard [`EventKind::TaskRemoveRequested`](crate::EventKind::TaskRemoveRequested).
//! A registry rejection, such as a duplicate task name, publishes the standard [`EventKind::TaskAddFailed`](crate::EventKind::TaskAddFailed).
//!
//! Events are best-effort observability. For one reliable final result, use `submit_and_watch` or `try_submit_and_watch`.
//!
//! ## Invariants
//!
//! - At most one registered runtime task may own a slot.
//! - Queued submissions are not handed to the runtime until they become the slot owner.
//! - `Queue` preserves FIFO order among submissions that remain pending.
//! - `Replace` creates or overwrites only the queue head; later items stay in order.
//! - The next owner starts only after reliable registry cleanup of the old owner.
//! - Runtime shutdown closes controller intake, resolves pending work, and joins the controller loop before the shared shutdown result is returned.
//!
//! A snapshot is a rolling observability view, not a transaction.
//! See [`ControllerSnapshot`] for its consistency limits.

mod view;
pub use view::{ControllerSnapshot, SlotStatusKind, SlotView};

mod admission;
pub use admission::AdmissionPolicy;

mod config;
pub use config::ControllerConfig;

mod core;
pub(crate) use core::Controller;

mod error;
pub use error::ControllerError;

mod spec;
pub use spec::ControllerSpec;

mod slot;
