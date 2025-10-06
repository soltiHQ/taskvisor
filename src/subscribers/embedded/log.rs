//! # LogWriter — human-readable event printer.
//!
//! A minimal subscriber that prints incoming [`Event`]s to stdout.
//! Useful for development, debugging, and demos.
//!
//! ## Example output
//! ```text
//! [001] [starting] task=worker attempt=1
//! [002] [failed] task=worker err="connection refused" attempt=1
//! [003] [backoff] task=worker delay=2s after_attempt=1 err="connection refused"
//! [004] [timeout] task=worker timeout=5s
//! [005] [stopped] task=worker
//! [006] [actor-exhausted] task=worker reason=policy
//! [007] [task-add-requested] task=new-worker
//! [008] [task-added] task=new-worker
//! [009] [task-remove-requested] task=old-worker
//! [010] [task-removed] task=old-worker
//! [011] [shutdown-requested]
//! [012] [all-stopped-within-grace]
//! ```
//!
//! ## Example
//! ```rust,ignore
//! use taskvisor::{Supervisor, Config, Subscribe, LogWriter};
//! use std::sync::Arc;
//!
//! let subs: Vec<Arc<dyn Subscribe>> = vec![Arc::new(LogWriter)];
//! let sup = Supervisor::new(Config::default(), subs);
//! ```

use crate::events::{Event, EventKind};
use crate::subscribers::Subscribe;
use async_trait::async_trait;

/// Human-readable event printer for stdout.
///
/// Prints each event with sequence number and relevant metadata.
/// Useful for development, debugging, and understanding event flow.
///
/// ## Output format
/// `[seq] [event-type] key=value ...`
///
/// ## Notes
/// - Unit struct, create with `LogWriter` (no constructor needed)
/// - Implements `Default` for convenience
pub struct LogWriter;

#[async_trait]
impl Subscribe for LogWriter {
    async fn on_event(&self, e: &Event) {
        let seq = format!("[{:03}]", e.seq % 1000);

        match e.kind {
            // === Shutdown events ===
            EventKind::GraceExceeded => {
                println!("{} [grace-exceeded]", seq);
            }
            EventKind::ShutdownRequested => {
                println!("{} [shutdown-requested]", seq);
            }
            EventKind::AllStoppedWithin => {
                println!("{} [all-stopped-within-grace]", seq);
            }

            // === Task lifecycle events ===
            EventKind::TaskStopped => {
                println!(
                    "{} [stopped] task={}",
                    seq,
                    e.task.as_deref().unwrap_or("none")
                );
            }
            EventKind::TimeoutHit => {
                println!(
                    "{} [timeout] task={} timeout={:?}",
                    seq,
                    e.task.as_deref().unwrap_or("none"),
                    e.timeout.unwrap_or_default()
                );
            }
            EventKind::TaskStarting => {
                println!(
                    "{} [starting] task={} attempt={}",
                    seq,
                    e.task.as_deref().unwrap_or("none"),
                    e.attempt.unwrap_or(0)
                );
            }
            EventKind::BackoffScheduled => {
                println!(
                    "{} [backoff] task={} delay={:?} after_attempt={} err={}",
                    seq,
                    e.task.as_deref().unwrap_or("none"),
                    e.delay.unwrap_or_default(),
                    e.attempt.unwrap_or(0),
                    e.error.as_deref().unwrap_or("none")
                );
            }
            EventKind::TaskFailed => {
                println!(
                    "{} [failed] task={} err={} attempt={}",
                    seq,
                    e.task.as_deref().unwrap_or("none"),
                    e.error.as_deref().unwrap_or("none"),
                    e.attempt.unwrap_or(0)
                );
            }

            // === Subscriber events ===
            EventKind::SubscriberOverflow => {
                println!(
                    "{} [subscriber-overflow] subscriber={} reason={}",
                    seq,
                    e.task.as_deref().unwrap_or("none"),
                    e.error.as_deref().unwrap_or("none")
                );
            }
            EventKind::SubscriberPanicked => {
                println!(
                    "{} [subscriber-panicked] subscriber={} info={}",
                    seq,
                    e.task.as_deref().unwrap_or("none"),
                    e.error.as_deref().unwrap_or("none")
                );
            }

            // === Runtime task management events ===
            EventKind::TaskAddRequested => {
                println!(
                    "{} [task-add-requested] task={}",
                    seq,
                    e.task.as_deref().unwrap_or("none")
                );
            }
            EventKind::TaskAdded => {
                println!(
                    "{} [task-added] task={}",
                    seq,
                    e.task.as_deref().unwrap_or("none")
                );
            }
            EventKind::TaskRemoveRequested => {
                println!(
                    "{} [task-remove-requested] task={}",
                    seq,
                    e.task.as_deref().unwrap_or("none")
                );
            }
            EventKind::TaskRemoved => {
                println!(
                    "{} [task-removed] task={}",
                    seq,
                    e.task.as_deref().unwrap_or("none")
                );
            }

            // === Actor terminal states ===
            EventKind::ActorExhausted => {
                println!(
                    "{} [actor-exhausted] task={} reason=policy",
                    seq,
                    e.task.as_deref().unwrap_or("none")
                );
            }
            EventKind::ActorDead => {
                println!(
                    "{} [actor-dead] task={} reason={}",
                    seq,
                    e.task.as_deref().unwrap_or("none"),
                    e.error.as_deref().unwrap_or("fatal")
                );
            }
        }
    }

    fn name(&self) -> &'static str {
        "LogWriter"
    }
}
