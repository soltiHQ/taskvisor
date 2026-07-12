//! # Controller
//!
//! Slot-based admission control for supervised tasks.
//!
//! The controller is an optional layer above direct task submission.
//! It accepts [`ControllerSpec`] values and decides when each task may enter the runtime.
//!
//! Use it when several submissions should share one sequential lane:
//! - skip or replace duplicate work while a slot is busy,
//! - only one deploy per environment,
//! - one job per customer/resource,
//! - one rebuild per index.
//!
//! ## Slot Model
//!
//! The controller admits by **slot**, not by task name.
//!
//! A slot is the key returned by [`ControllerSpec::slot_name`].
//! If no slot is set, it defaults to the task name.
//! Use [`ControllerSpec::with_slot`] to group several different task names into one slot.
//!
//! Task names still belong to the runtime registry and must be unique among currently registered tasks.
//!
//! ```text
//! task "deploy-main-42" ┐
//! task "deploy-main-43" ├── slot "deploy-main"
//! task "deploy-main-44" ┘
//! ```
//!
//! All three tasks are different runtime tasks, but the controller admits them through the same slot.
//!
//! ## Admission Policies
//!
//! When a slot is idle, every policy admits the submission.
//! When a slot is busy, the policy decides what happens.
//!
//! | Policy                             | Busy slot behavior                                         |
//! |------------------------------------|------------------------------------------------------------|
//! | [`AdmissionPolicy::Queue`]         | enqueue behind older queued submissions                    |
//! | [`AdmissionPolicy::Replace`]       | keep the latest next submission; supersede the queued head |
//! | [`AdmissionPolicy::DropIfRunning`] | reject the submission                                      |
//!
//! A rejected watched submission resolves to [`TaskOutcome::Rejected`](crate::TaskOutcome::Rejected).
//!
//! ## Slot States
//!
//! ```text
//! Idle
//!   submit
//!   │
//!   ▼
//! Admitting ── Add reply Ok ──► Running ── registry completion ──► Idle or next queued task
//!   └── Add reply Err ────────► Idle or next queued task
//!
//! Running + Replace
//!   └── request remove ──► Terminating ── registry completion ──► next queued task
//! ```
//!
//! The controller starts the next queued task only after reliable registry completion.
//! At that point the previous actor is joined and its registry id and task name are released.
//! Lifecycle events remain available for observability, but they do not decide slot state.
//!
//! ## Public Surface
//!
//! - Build submissions with [`ControllerSpec::queue`], [`ControllerSpec::replace`], or [`ControllerSpec::drop_if_running`].
//! - Configure with `Supervisor::builder(...).with_controller(ControllerConfig)`.
//! - Submit with `SupervisorHandle::submit`, `try_submit`, or `submit_and_watch`.
//! - Inspect live slot state with `SupervisorHandle::controller_snapshot`.
//!
//! ## Events
//!
//! The controller publishes:
//! - [`EventKind::ControllerSlotTransition`](crate::EventKind::ControllerSlotTransition)
//! - [`EventKind::ControllerSubmitted`](crate::EventKind::ControllerSubmitted)
//! - [`EventKind::ControllerRejected`](crate::EventKind::ControllerRejected)
//!
//! Events are observability. For guaranteed final result of a watched submission, use `submit_and_watch`.
//!
//! ## Invariants
//!
//! - At most one runtime task may occupy a slot.
//! - Queued submissions are not handed to the runtime until they become the slot owner.
//! - `Queue` is FIFO.
//! - `Replace` is latest-wins for the next queued submission.
//! - Slot advancement is gated on reliable terminal registry cleanup.

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
