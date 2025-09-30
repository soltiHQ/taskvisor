//! # Event system for task lifecycle notifications
//!
//! This module provides the core event infrastructure for taskvisor:
//! - [`Event`] - runtime event with metadata (timestamp, task name, error, etc.)
//! - [`EventKind`] - classification of event types (startup, failure, shutdown, etc.)
//! - [`Bus`] - broadcast channel for publishing events to multiple subscribers
//!
//! Events flow from task actors through the bus to all subscribers:
//! ```text
//! TaskActor → Bus.publish(Event) → broadcast → all subscribers
//! ```

mod bus;
mod event;

pub use bus::Bus;
pub use event::{Event, EventKind};
