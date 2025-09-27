//! # Observer: user-facing event handlers
//!
//! The [`Observer`] trait is the main **extension point** for end users.
//! All runtime [`Event`]s flow through the bus and into observers.
//!
//! Implementing your own observer allows you to plug in:
//! - metrics export (Prometheus, OpenTelemetry, …);
//! - custom monitoring or alerting pipelines;
//! - structured logging;
//! - other usage.
//!
//! # High-level architecture:
//! ```text
//! Event flow:
//!   TaskActor ── publish(Event) ──► Bus ──► Supervisor.observer_listener()
//!                                              └─► Observer::on_event(&Event)
//!
//! User-defined observers:
//!   - implement [`Observer`] trait
//!   - receive every [`Event`] from the bus
//!   - run custom logic asynchronously
//!
//! Provided implementations:
//!   - [`LoggerObserver`] (enabled via `logging` feature) → prints events to stdout
//!
//!   TaskActor ... ──► Bus ──► Observer::on_event(&Event)
//!                                      │
//!              ┌───────────────────────┼───────────────────────┐
//!              ▼                       ▼                       ▼
//!      LoggerObserver            MetricsObserver         CustomObserver
//!        (stdout)                  (Prometheus, OTEL)     (user logic)
//! ```
//!
//! #### Note:
//! A simple [`LoggerObserver`] is available (enabled via the `logging` feature), useful for debug and testing.
//!
//! # Example: custom observer
//! ```no_run
//! use taskvisor::{Observer, Event, EventKind};
//! use async_trait::async_trait;
//!
//! // Define your own observer
//! struct MetricsObserver;
//!
//! #[async_trait]
//! impl Observer for MetricsObserver {
//!     async fn on_event(&self, event: &Event) {
//!         match event.kind {
//!             EventKind::TaskStarting => {
//!                 println!("[metrics] task started: {:?}", event.task);
//!             }
//!             EventKind::TaskFailed => {
//!                 println!("[metrics] task failed: {:?}, error={:?}", event.task, event.error);
//!             }
//!             _ => { /* ignore others */ }
//!         }
//!     }
//! }
//!
//! # async fn demo(ev: Event) {
//! let obs = MetricsObserver;
//! obs.on_event(&ev).await;
//! # }
//! ```

use crate::event::{Event, EventKind};
use async_trait::async_trait;

/// # Trait for receiving runtime events from the bus.
///
/// Observers are called asynchronously by the supervisor whenever a new [`Event`] is published. Typical use cases include:
/// - forwarding to metrics systems;
/// - triggering side effects
/// - structured logging.
#[async_trait]
pub trait Observer {
    /// Called for every emitted [`Event`].
    async fn on_event(&self, event: &Event);
}


/// Base observer that logs events to stdout.
///
/// Enabled via the `logging` feature. Useful for demos and debugging.
pub struct LoggerObserver;

#[async_trait]
impl Observer for LoggerObserver {
    async fn on_event(&self, e: &Event) {
        match e.kind {
            EventKind::TaskStarting => {
                if let (Some(task), Some(att)) = (&e.task, e.attempt) {
                    println!("[starting] task={task} attempt={att}");
                }
            }
            EventKind::TaskFailed => {
                println!(
                    "[failed] task={:?} err={:?} attempt={:?}",
                    e.task, e.error, e.attempt
                );
            }
            EventKind::TaskStopped => {
                println!("[stopped] task={:?}", e.task);
            }
            EventKind::ShutdownRequested => {
                println!("[shutdown-requested]");
            }
            EventKind::AllStoppedWithin => {
                println!("[all-stopped-within-grace]");
            }
            EventKind::GraceExceeded => {
                println!("[grace-exceeded]");
            }
            EventKind::BackoffScheduled => {
                println!(
                    "[backoff] task={:?} delay={:?} after_attempt={:?} err={:?}",
                    e.task, e.delay, e.attempt, e.error
                );
            }
            EventKind::TimeoutHit => {
                println!("[timeout] task={:?} timeout={:?}", e.task, e.timeout);
            }
        }
    }
}
