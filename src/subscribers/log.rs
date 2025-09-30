//! # Simple logging subscriber for debugging and demos.
//!
//! [`LogWriter`] prints events to stdout in a human-readable format.
//! This is primarily useful for development, debugging, and examples.
//!
//! ## Output format
//! ```text
//! [starting] task=worker attempt=1
//! [failed] task=worker err="connection refused" attempt=1
//! [backoff] task=worker delay=2s after_attempt=1 err="connection refused"
//! [timeout] task=worker timeout=5s
//! [stopped] task=worker
//! [shutdown-requested]
//! [all-stopped-within-grace]
//! ```
//!
//! ## Example
//! ```no_run
//! # use taskvisor::{Supervisor, Config, LogWriter};
//! let supervisor = Supervisor::new(Config::default(), LogWriter);
//! // LogWriter will print all events to stdout
//! ```

use crate::Subscriber;
use crate::events::{Event, EventKind};
use async_trait::async_trait;

/// Simple stdout logging subscriber.
///
/// Enabled via the `logging` feature. Prints human-readable event descriptions
/// to stdout for debugging and demonstration purposes.
///
/// Not intended for production use - implement a custom [`Subscriber`] for
/// structured logging or metrics collection.
pub struct LogWriter;

#[async_trait]
impl Subscriber for LogWriter {
    async fn handle(&self, e: &Event) {
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
