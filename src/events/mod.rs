//! Runtime events: types and broadcast bus.
//!
//! This module groups the event **data model** and the **bus** used to publish/subscribe to runtime events
//! emitted by the supervisor, registry, task actors, runner and subscriber workers.
//!
//! ## Contents
//!
//! - [`EventKind`], [`Event`] event classification and payload metadata
//! - [`Bus`] thin wrapper over `tokio::sync::broadcast`
//!
//! ## Flow
//!
//! ```text
//! TaskAddRequested → TaskAdded → TaskStarting ──┬── TaskStopped ─► ActorExhausted
//!                                               ├── TaskFailed  ─► BackoffScheduled ─► TaskStarting
//!                                               ├── TimeoutHit  ─► BackoffScheduled ─► TaskStarting
//!                                               └── Fatal       ─► ActorDead
//!
//! TaskRemoveRequested → TaskStopped → TaskRemoved
//!
//! ShutdownRequested → AllStoppedWithinGrace | GraceExceeded
//! ```
//!
//! ## Wiring
//!
//! Events are consumed by user-defined [`Subscribe`](crate::Subscribe) implementations.
//! See [`LogWriter`](crate::LogWriter) for a built-in example.

mod event;
pub use event::{BackoffSource, Event, EventKind};

mod bus;
pub(crate) use bus::Bus;
