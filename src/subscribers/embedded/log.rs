//! # LogWriter — simple event printer
//!
//! A minimal subscriber that prints incoming [`Event`]s to stdout.
//! Use it for test or demo.
//!
//! ## Example output
//! ```text
//! [starting] task="worker" attempt=1
//! [failed] task="worker" err="connection refused" attempt=1
//! [backoff] task="worker" delay=2s after_attempt=1 err="connection refused"
//! [timeout] task="worker" timeout=5s
//! [stopped] task="worker"
//! [shutdown-requested]
//! [all-stopped-within-grace]
//! [grace-exceeded]
//! ```

use crate::events::{Event, EventKind};
use crate::subscribers::Subscribe;
use async_trait::async_trait;

/// Event writer subscriber.
#[derive(Default)]
pub struct LogWriter;

impl LogWriter {
    /// Construct a new [`LogWriter`].
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Subscribe for LogWriter {
    async fn on_event(&self, e: &Event) {
        match e.kind {
            EventKind::GraceExceeded => {
                println!("[grace-exceeded]");
            }
            EventKind::ShutdownRequested => {
                println!("[shutdown-requested]");
            }
            EventKind::AllStoppedWithin => {
                println!("[all-stopped-within-grace]");
            }
            EventKind::TaskStopped => {
                println!("[stopped] task={:?}", e.task);
            }
            EventKind::TimeoutHit => {
                println!("[timeout] task={:?} timeout={:?}", e.task, e.timeout);
            }
            EventKind::TaskStarting => {
                println!("[starting] task={:?} attempt={:?}", e.task, e.attempt);
            }
            EventKind::BackoffScheduled => {
                println!(
                    "[backoff] task={:?} delay={:?} after_attempt={:?} err={:?}",
                    e.task, e.delay, e.attempt, e.error
                );
            }
            EventKind::SubscriberOverflow => {
                println!(
                    "[subscriber-overflow] subscriber={:?} reason={:?}",
                    e.task, e.error
                );
            }
            EventKind::TaskFailed => {
                println!(
                    "[failed] task={:?} err={:?} attempt={:?}",
                    e.task, e.error, e.attempt
                );
            }
            EventKind::SubscriberPanicked => {
                println!(
                    "[subscriber-panicked] subscriber={} info={}",
                    e.task.as_deref().unwrap_or("unknown"),
                    e.error.as_deref().unwrap_or("unknown"),
                );
            }
        }
    }

    fn name(&self) -> &'static str {
        "LogWriter"
    }
}
