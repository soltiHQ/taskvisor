use crate::{Event, EventKind, Observer};
use async_trait::async_trait;

/// Base observer that logs events to stdout.
///
/// Enabled via the `logging` feature. Useful for demos and debugging.
pub struct LogWriter;

#[async_trait]
impl Observer for LogWriter {
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
