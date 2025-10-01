//! # LogWriter – simple printer for events
//!
//! **Purpose:** human-readable output for demos/tests. For production, prefer
//! structured logging (`tracing`) or your observability stack.
//!
//! ## Typical output
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

use async_trait::async_trait;

use crate::events::{Event, EventKind};
use crate::subscribers::Subscribe;

/// Human-readable printer. Queue size is configurable via builder.
pub struct LogWriter {
    compact: bool,
    capacity: usize,
}

impl LogWriter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            compact: true,
            capacity: 1024,
        }
    }

    #[must_use]
    pub fn with_compact(mut self, compact: bool) -> Self {
        self.compact = compact;
        self
    }

    #[must_use]
    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity.max(1);
        self
    }
}

#[async_trait]
impl Subscribe for LogWriter {
    async fn on_event(&self, e: &Event) {
        match e.kind {
            EventKind::TaskStarting => {
                println!("[starting] task={:?} attempt={:?}", e.task, e.attempt);
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
        if !self.compact {
            println!("{:#?}", e);
        }
    }

    fn name(&self) -> &'static str {
        "LogWriter"
    }
    fn queue_capacity(&self) -> usize {
        self.capacity
    }
}

impl Default for LogWriter {
    fn default() -> Self {
        Self::new()
    }
}
