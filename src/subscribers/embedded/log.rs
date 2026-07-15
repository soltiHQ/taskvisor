//! # Simple stdout event printer
//!
//! [`LogWriter`] prints incoming [`Event`] values to standard output.
//! It is useful for examples, local development, and debugging.
//!
//! For production structured logs, prefer [`TracingBridge`](crate::TracingBridge) with the`tracing` feature.
//!
//! Each line starts with the event's stable label from [`EventKind::as_label`].
//!
//! ## Example output
//! ```text
//! [001] [task_starting] task=worker attempt=1
//! [002] [task_failed] task=worker reason="connection refused" attempt=1
//! [003] [backoff_scheduled] task=worker source=failure delay=2s after_attempt=1 reason="connection refused"
//! [004] [timeout_hit] task=worker timeout=5s
//! [005] [task_stopped] task=worker
//! [006] [actor_exhausted] task=worker reason=policy
//! [007] [task_add_requested] task=new-worker
//! [008] [task_added] task=new-worker
//! [009] [task_remove_requested] task=old-worker
//! [010] [task_removed] task=old-worker
//! [011] [shutdown_requested]
//! [012] [all_stopped_within_grace]
//! ```
//!
//! ## Example
//! ```rust,no_run
//! use taskvisor::{Supervisor, SupervisorConfig, Subscribe, LogWriter};
//! use std::sync::Arc;
//!
//! let subs: Vec<Arc<dyn Subscribe>> = vec![Arc::new(LogWriter::default())];
//! let sup = Supervisor::new(SupervisorConfig::default(), subs);
//! ```

use crate::events::{Event, EventKind};
use crate::subscribers::Subscribe;

/// Prints human-readable events to standard output.
///
/// Implements [`Subscribe`] and prints `[seq] [event-type] key=value ...` with relevant metadata.
/// Output is intended for people and is not a stable machine-readable format.
///
/// ## Also
///
/// - See [`Subscribe`] for the subscriber contract and queue/overflow semantics.
/// - See [`Event`] and [`EventKind`] for event structure.
#[cfg_attr(docsrs, doc(cfg(feature = "logging")))]
#[derive(Default)]
pub struct LogWriter;

impl Subscribe for LogWriter {
    fn on_event(&self, e: &Event) {
        self.print_event(e);
    }

    fn name(&self) -> &str {
        "LogWriter"
    }
}

impl LogWriter {
    fn print_event(&self, e: &Event) {
        let head = event_head(e);

        fn fmt_ms(ms: Option<u32>) -> String {
            match ms {
                Some(v) if v >= 1000 && v % 1000 == 0 => format!("{}s", v / 1000),
                Some(v) if v >= 1000 => format!("{:.1}s", v as f64 / 1000.0),
                Some(v) => format!("{}ms", v),
                None => "0ms".to_string(),
            }
        }
        fn or<'a>(s: Option<&'a str>, def: &'a str) -> &'a str {
            s.unwrap_or(def)
        }

        match e.kind {
            // Shutdown: no payload.
            EventKind::ShutdownRequested
            | EventKind::AllStoppedWithinGrace
            | EventKind::GraceExceeded => {
                println!("{head}");
            }

            // Task lifecycle and management: task name only.
            EventKind::TaskStopped
            | EventKind::TaskCanceled
            | EventKind::TaskAddRequested
            | EventKind::TaskAdded
            | EventKind::TaskRemoveRequested
            | EventKind::TaskRemoved => {
                println!("{head} task={}", or(e.task.as_deref(), "none"));
            }

            EventKind::TaskStarting => {
                println!(
                    "{head} task={} attempt={}",
                    or(e.task.as_deref(), "none"),
                    e.attempt.unwrap_or(0)
                );
            }
            EventKind::TaskFailed => {
                println!(
                    "{head} task={} reason=\"{}\" attempt={}",
                    or(e.task.as_deref(), "none"),
                    or(e.reason.as_deref(), "unknown"),
                    e.attempt.unwrap_or(0)
                );
            }
            EventKind::TaskAddFailed => {
                println!(
                    "{head} task={} reason=\"{}\"",
                    or(e.task.as_deref(), "none"),
                    or(e.reason.as_deref(), "unknown")
                );
            }
            EventKind::TimeoutHit => {
                println!(
                    "{head} task={} timeout={}",
                    or(e.task.as_deref(), "none"),
                    fmt_ms(e.timeout_ms)
                );
            }
            EventKind::BackoffScheduled => {
                let src = e.backoff_source.map_or("unknown", |s| s.as_label());
                println!(
                    "{head} task={} source={} delay={} after_attempt={} reason=\"{}\"",
                    or(e.task.as_deref(), "none"),
                    src,
                    fmt_ms(e.delay_ms),
                    e.attempt.unwrap_or(0),
                    or(e.reason.as_deref(), "none")
                );
            }

            // Subscribers: the `task` field carries the subscriber name.
            EventKind::SubscriberOverflow | EventKind::SubscriberPanicked => {
                println!(
                    "{head} subscriber={} reason=\"{}\"",
                    or(e.task.as_deref(), "none"),
                    or(e.reason.as_deref(), "unknown")
                );
            }
            EventKind::RuntimeFailure => {
                println!(
                    "{head} component={} reason=\"{}\"",
                    or(e.task.as_deref(), "none"),
                    or(e.reason.as_deref(), "unknown")
                );
            }

            // Terminals.
            EventKind::ActorExhausted => {
                println!(
                    "{head} task={} reason=\"{}\"",
                    or(e.task.as_deref(), "none"),
                    or(e.reason.as_deref(), "policy")
                );
            }
            EventKind::ActorDead => {
                println!(
                    "{head} task={} reason=\"{}\"",
                    or(e.task.as_deref(), "none"),
                    or(e.reason.as_deref(), "fatal")
                );
            }

            // Controller: the `task` field carries the slot name.
            #[cfg(feature = "controller")]
            EventKind::ControllerRejected | EventKind::ControllerSlotTransition => {
                println!(
                    "{head} slot={} reason=\"{}\"",
                    or(e.task.as_deref(), "none"),
                    or(e.reason.as_deref(), "unknown")
                );
            }
            #[cfg(feature = "controller")]
            EventKind::ControllerSubmitted => {
                println!(
                    "{head} slot={} {}",
                    or(e.task.as_deref(), "none"),
                    or(e.reason.as_deref(), "")
                );
            }
        }
    }
}

fn event_head(e: &Event) -> String {
    format!("[{:03}] [{}]", e.seq, e.kind.as_label())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_head_keeps_the_full_sequence_number() {
        let mut event = Event::new(EventKind::TaskStarting);
        event.seq = 12_345;

        assert_eq!(event_head(&event), "[12345] [task_starting]");
    }
}
