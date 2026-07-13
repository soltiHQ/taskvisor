//! # Slot-based admission control
//!
//! The controller is an optional layer before the supervisor registry. It
//! decides when a submitted task may be registered. Use direct `add*` methods
//! when you do not need this admission step.
//!
//! Use the controller when work with the same key must not overlap:
//!
//! - one deployment per environment;
//! - one job per customer or resource;
//! - one rebuild per index;
//! - skip or replace duplicate work while a lane is busy.
//!
//! ## Slots and task names
//!
//! A **slot** is one sequential admission lane. The controller admits by slot,
//! not by task name.
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
//! When a slot is idle, every policy starts admission. When it is busy, the
//! policy decides what happens to the new submission.
//!
//! | Policy                             | Busy slot behavior                                         |
//! |------------------------------------|------------------------------------------------------------|
//! | [`AdmissionPolicy::Queue`]         | enqueue behind older queued submissions                    |
//! | [`AdmissionPolicy::Replace`]       | retire the owner and keep the latest next submission       |
//! | [`AdmissionPolicy::DropIfRunning`] | reject the submission                                      |
//!
//! `Replace` changes only the next queued item. FIFO items already behind it
//! stay in the queue. A watched submission that is rejected resolves to
//! [`TaskOutcome::Rejected`](crate::TaskOutcome::Rejected).
//!
//! ## Slot states
//!
//! ```text
//!             submit
//! Idle -----------------> Admitting
//!  ^                         |       \
//!  |                         | OK     \ rejected
//!  |                         v         \
//!  +-- terminal cleanup -- Running     +--> next item or Idle
//!                            |
//!                            | Replace / remove
//!                            v
//!                        Terminating
//!                            |
//!                            +-- terminal cleanup --> next item or Idle
//! ```
//!
//! Admitting + Replace is shown as `Terminating` in public snapshots. Taskvisor
//! first waits for the registration decision. If registration succeeds, it then
//! removes that owner before starting the replacement.
//!
//! The next queued task starts only after the registry confirms terminal
//! cleanup. At that point the previous task runner is joined and its task name is
//! free. Events are only for observability; they do not drive slot state.
//!
//! ## Public API
//!
//! - Build submissions with [`ControllerSpec::queue`], [`ControllerSpec::replace`], or [`ControllerSpec::drop_if_running`].
//! - Configure with `Supervisor::builder(...).with_controller(ControllerConfig)`.
//! - Submit with `SupervisorHandle::submit`, `try_submit`, `submit_and_watch`, or
//!   `try_submit_and_watch`.
//! - Remove or cancel by the [`TaskId`](crate::TaskId) returned from submission.
//! - Read current slot state with `SupervisorHandle::controller_snapshot`.
//!
//! `submit*` returning `Ok(id)` confirms only that the controller command queue
//! accepted the submission. Slot admission and registry registration happen
//! later. Use `submit_and_watch` or `try_submit_and_watch` when the final result
//! matters.
//!
//! The returned `TaskId` is allocated before runtime admission. If admission
//! succeeds, the runtime uses the same ID. Before admission, it still identifies
//! queued work for cancellation, outcomes, and event correlation.
//!
//! [`ControllerConfig::queue_capacity`] bounds the controller command queue.
//! [`ControllerConfig::max_slot_queue`] separately bounds FIFO `Queue` items
//! waiting behind a busy slot. `Replace` may still occupy the next-item position
//! when the FIFO capacity is zero.
//!
//! A watched submission that is never registered resolves to
//! [`TaskOutcome::Rejected`](crate::TaskOutcome::Rejected). After registration,
//! its waiter resolves to the runtime task's terminal outcome.
//!
//! ## Events
//!
//! The controller publishes:
//! - [`EventKind::ControllerSlotTransition`](crate::EventKind::ControllerSlotTransition)
//! - [`EventKind::ControllerSubmitted`](crate::EventKind::ControllerSubmitted)
//! - [`EventKind::ControllerRejected`](crate::EventKind::ControllerRejected)
//!
//! Events are best-effort observability. For one reliable final result, use
//! `submit_and_watch` or `try_submit_and_watch`.
//!
//! ## Invariants
//!
//! - At most one registered runtime task may own a slot.
//! - Queued submissions are not handed to the runtime until they become the slot owner.
//! - `Queue` is FIFO.
//! - `Replace` is latest-wins for the next queued item.
//! - The next owner starts only after reliable registry cleanup of the old owner.
//! - Runtime shutdown closes controller intake, resolves pending work, and joins the controller
//!   loop before the shared shutdown result is returned.
//!
//! A snapshot is point-in-time observability, not a transaction. See
//! [`ControllerSnapshot`] for its consistency limits.

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
