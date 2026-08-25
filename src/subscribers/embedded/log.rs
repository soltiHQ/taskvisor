//! Implements the `logging` feature's human-readable event endpoint.
//!
//! [`LogWriter`] is a [`Subscribe`] implementation at the end of the best-effort observability path.
//!
//! ```text
//! runtime event relay ──► subscriber queue ──► LogWriter ──► standard output
//! ```
//!
//! Each line starts with the event sequence and the stable [`EventKind::as_label`] value.
//! Event-specific fields follow as `key=value`. Free-form text is quoted, escaped, and truncated after 4096 characters.
//! The complete line format is intended for people and is not a stable data format.
//! It is not a complete serialization of [`Event`]; use a custom subscriber or `TracingBridge` when every typed field is needed.

use crate::events::{Event, EventKind};
use crate::subscribers::Subscribe;

const MAX_VALUE_CHARS: usize = 4096;

fn format_value(value: &str) -> String {
    let mut chars = value.chars();
    let mut value = chars.by_ref().take(MAX_VALUE_CHARS).collect::<String>();
    if chars.next().is_some() {
        value.push_str("…[truncated]");
    }
    format!("{value:?}")
}

/// Prints each received event as one readable line on standard output.
///
/// This type uses the queue, loss, panic, and shutdown contract defined by [`Subscribe`].
///
/// The output is designed for local visibility. Do not parse it as an API or rely on its field
/// set for task correlation. It omits some [`Event`] fields, including `id` and `at`.
///
/// # Examples
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use taskvisor::{LogWriter, Subscribe, Supervisor, SupervisorConfig};
///
/// let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::new(LogWriter)];
/// let supervisor = Supervisor::new(SupervisorConfig::default(), subscribers);
/// ```
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
            EventKind::AttemptSucceeded
            | EventKind::AttemptCanceled
            | EventKind::TaskAddRequested
            | EventKind::TaskAdded
            | EventKind::TaskRemoveRequested
            | EventKind::TaskRemoved => {
                println!(
                    "{head} task={}",
                    format_value(or(e.task.as_deref(), "none"))
                );
            }

            EventKind::AttemptStarting => {
                println!(
                    "{head} task={} attempt={}",
                    format_value(or(e.task.as_deref(), "none")),
                    e.attempt.unwrap_or(0)
                );
            }
            EventKind::AttemptFailed => {
                println!(
                    "{head} task={} reason={} attempt={}",
                    format_value(or(e.task.as_deref(), "none")),
                    format_value(or(e.reason.as_deref(), "unknown")),
                    e.attempt.unwrap_or(0)
                );
            }
            EventKind::TaskAddFailed => {
                println!(
                    "{head} task={} reason={}",
                    format_value(or(e.task.as_deref(), "none")),
                    format_value(or(e.reason.as_deref(), "unknown"))
                );
            }
            EventKind::AttemptTimedOut => {
                println!(
                    "{head} task={} timeout={}",
                    format_value(or(e.task.as_deref(), "none")),
                    fmt_ms(e.timeout_ms)
                );
            }
            EventKind::BackoffScheduled => {
                let src = e.backoff_source.map_or("unknown", |s| s.as_label());
                println!(
                    "{head} task={} source={} delay={} after_attempt={} reason={}",
                    format_value(or(e.task.as_deref(), "none")),
                    src,
                    fmt_ms(e.delay_ms),
                    e.attempt.unwrap_or(0),
                    format_value(or(e.reason.as_deref(), "none"))
                );
            }
            EventKind::SubscriberOverflow => {
                println!(
                    "{head} subscriber={} reason={} dropped={}",
                    format_value(or(e.task.as_deref(), "none")),
                    format_value(or(e.reason.as_deref(), "unknown")),
                    e.dropped.unwrap_or(0)
                );
            }
            EventKind::SubscriberPanicked => {
                println!(
                    "{head} subscriber={} reason={}",
                    format_value(or(e.task.as_deref(), "none")),
                    format_value(or(e.reason.as_deref(), "unknown"))
                );
            }
            EventKind::RuntimeFailure => {
                println!(
                    "{head} component={} reason={}",
                    format_value(or(e.task.as_deref(), "none")),
                    format_value(or(e.reason.as_deref(), "unknown"))
                );
            }
            EventKind::OwnershipCapacityRetired => {
                println!("{}", ownership_capacity_retired_line(e));
            }

            // Registered-task terminal outcome.
            EventKind::TaskFinished => {
                let task = or(e.task.as_deref(), "none");
                let outcome = e
                    .outcome_kind
                    .map(|kind| kind.as_label())
                    .unwrap_or("unknown");
                if let Some(reason) = e.reason.as_deref() {
                    println!(
                        "{head} task={} outcome={outcome} reason={}",
                        format_value(task),
                        format_value(reason)
                    );
                } else {
                    println!("{head} task={} outcome={outcome}", format_value(task));
                }
            }

            // Controller: the `task` field carries the slot name.
            #[cfg(feature = "controller")]
            EventKind::ControllerRejected => {
                println!(
                    "{head} slot={} rejection={} reason={}",
                    format_value(or(e.task.as_deref(), "none")),
                    e.rejection_kind
                        .map(|kind| kind.as_label())
                        .unwrap_or("unknown"),
                    format_value(or(e.reason.as_deref(), "unknown"))
                );
            }
            #[cfg(feature = "controller")]
            EventKind::ControllerSlotTransition => {
                println!(
                    "{head} slot={} reason={}",
                    format_value(or(e.task.as_deref(), "none")),
                    format_value(or(e.reason.as_deref(), "unknown"))
                );
            }
            #[cfg(feature = "controller")]
            EventKind::ControllerSubmitted => {
                println!(
                    "{head} slot={} reason={}",
                    format_value(or(e.task.as_deref(), "none")),
                    format_value(or(e.reason.as_deref(), ""))
                );
            }
        }
    }
}

fn event_head(e: &Event) -> String {
    format!("[{:03}] [{}]", e.seq, e.kind.as_label())
}

fn ownership_capacity_retired_line(e: &Event) -> String {
    format!(
        "{} component={} configured_capacity={} effective_capacity={} retired_units={}",
        event_head(e),
        format_value(e.task.as_deref().unwrap_or("none")),
        e.configured_capacity.unwrap_or(0),
        e.effective_capacity.unwrap_or(0),
        e.retired_units.unwrap_or(0)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_head_keeps_the_full_sequence_number() {
        let mut event = Event::new(EventKind::AttemptStarting);
        event.seq = 12_345;

        assert_eq!(event_head(&event), "[12345] [attempt_starting]");
    }

    #[test]
    fn values_are_escaped_and_bounded() {
        assert_eq!(
            format_value("first\nsecond\t\"quoted\""),
            "\"first\\nsecond\\t\\\"quoted\\\"\""
        );

        let long = "x".repeat(MAX_VALUE_CHARS + 1);
        let rendered = format_value(&long);
        assert!(rendered.ends_with("…[truncated]\""));
        assert!(!rendered.contains('\n'));
    }

    #[test]
    fn ownership_retirement_prints_every_capacity_value() {
        let mut event =
            Event::ownership_capacity_retired(16, 14, 2).with_task("destructor_isolation");
        event.seq = 42;

        assert_eq!(
            ownership_capacity_retired_line(&event),
            "[042] [ownership_capacity_retired] component=\"destructor_isolation\" configured_capacity=16 effective_capacity=14 retired_units=2"
        );
    }
}
