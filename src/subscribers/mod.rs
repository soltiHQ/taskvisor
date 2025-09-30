//! # Event subscribers for the taskvisor runtime.
//!
//! This module provides the [`Subscriber`] trait and built-in implementations
//! for handling runtime events broadcast through the [`Bus`](crate::events::Bus).
//!
//! ## Architecture
//! ```text
//! Event flow:
//!   TaskActor ── publish(Event) ──► Bus ──► broadcast to all subscribers
//!                                              │
//!                                              ├──► Subscriber::handle(&Event)
//!                                              │         │
//!                                              │    ┌────┴────┬─────────┬───────┐
//!                                              │    ▼         ▼         ▼       ▼
//!                                              │  LogWriter  Metrics  Custom  ...
//!                                              │
//!                                              └──► AliveTracker (internal state tracking)
//! ```
//!
//! ## Subscriber types
//! - **Passive subscribers** - observe and react to events (logging, metrics, alerts)
//! - **Stateful subscribers** - maintain internal state based on events (AliveTracker)
//!
//! ## Implementing custom subscribers
//! ```no_run
//! use taskvisor::{Subscriber, Event, EventKind};
//! use async_trait::async_trait;
//!
//! struct MetricsSubscriber;
//!
//! #[async_trait]
//! impl Subscriber for MetricsSubscriber {
//!     async fn handle(&self, event: &Event) {
//!         match event.kind {
//!             EventKind::TaskFailed => {
//!                 // increment failure counter
//!             }
//!             _ => {}
//!         }
//!     }
//! }
//! ```

mod alive;
mod log;
mod subscriber;

pub use alive::AliveTracker;
pub use log::LogWriter;
pub use subscriber::Subscriber;
