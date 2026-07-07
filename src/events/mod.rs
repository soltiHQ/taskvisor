//! # Runtime events.
//!
//! This module contains the event data model used by taskvisor.
//!
//! Events describe task lifecycle, runtime management, shutdown, subscriber diagnostics, and optional controller activity.
//!
//! | Type              | Role                                       |
//! |-------------------|--------------------------------------------|
//! | [`EventKind`]     | Event classification                       |
//! | [`Event`]         | Event payload and metadata                 |
//! | [`BackoffSource`] | Why a `BackoffScheduled` event was emitted |
//!
//! Events are delivered through an internal broadcast bus.
//! Delivery is best-effort: slow consumers may miss events.
//! Use events for observability, not as the only source of correctness.
//!
//! ## Common Flow
//!
//! ```text
//! Add:
//!   TaskAddRequested ──► TaskAdded ──► TaskStarting
//!                   └──► TaskAddFailed
//!
//! Attempt:
//!   TaskStarting ──► TaskStopped
//!              ├──► TaskCanceled
//!              ├──► TaskFailed
//!              └──► TimeoutHit ──► TaskFailed
//!
//! After success:
//!   TaskStopped ──► ActorExhausted
//!              └──► BackoffScheduled(Success) ──► TaskStarting
//!
//! After retryable failure:
//!   TaskFailed ──► BackoffScheduled(Failure) ──► TaskStarting
//!             └──► ActorExhausted
//!
//! After fatal failure:
//!   TaskFailed ──► ActorDead
//!
//! Remove:
//!   TaskRemoveRequested ──► TaskRemoved
//!
//! Shutdown:
//!   ShutdownRequested ──► AllStoppedWithinGrace | GraceExceeded
//! ```
//!
//! [`TaskRemoved`](EventKind::TaskRemoved) is a registry cleanup confirmation.
//! It is emitted after the task actor has been joined or cleaned up.
//!
//! ## Subscribers
//!
//! User code consumes events by implementing [`Subscribe`](crate::Subscribe).
//! With the `logging` feature, `LogWriter` provides a small built-in example.

mod event;
pub use event::{BackoffSource, Event, EventKind};

mod bus;
pub(crate) use bus::Bus;
