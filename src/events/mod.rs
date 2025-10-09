//! Runtime events: types and broadcast bus.
//!
//! This module groups the event **data model** and the **bus** used to
//! publish/subscribe to runtime events emitted by the supervisor, registry,
//! task actors, runner and subscriber workers.
//!
//! ## Contents
//! - [`EventKind`], [`Event`] event classification and payload metadata
//! - [`Bus`] thin wrapper over `tokio::sync::broadcast`
//!
//! ## Quick reference
//! - **Publishers**: `Supervisor`, `Registry`, `TaskActor`, `runner::run_once`,
//!   `SubscriberSet` workers (overflow/panic).
//! - **Consumers**: `Supervisor::subscriber_listener()` (fans out to `SubscriberSet`
//!   and updates `AliveTracker`), and `Registry` (its own listener).
//!
//! See `core/mod.rs` for the system-level wiring diagram.

mod bus;
mod event;

pub use bus::Bus;
pub use event::{BackoffSource, Event, EventKind};
