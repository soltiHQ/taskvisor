//! # Bridge to `tracing`
//!
//! [`TracingBridge`] converts every event it receives into one structured [`tracing`] event.
//!
//! Each tracing event uses target `taskvisor` and contains:
//! - a level based on the event severity (see [`TracingBridge`]),
//! - structured fields: `event` (the stable label), `seq`, and the optional payload fields that are set
//!   (`task`, `id`, `attempt`, `reason`, `delay_ms`, `timeout_ms`, `duration_ms`, `exit_code`, `backoff_source`).
//!
//! Unset optional fields are not recorded.
//!
//! ## Example
//! ```rust,no_run
//! use std::sync::Arc;
//! use taskvisor::{Subscribe, Supervisor, SupervisorConfig, TracingBridge};
//!
//! let subs: Vec<Arc<dyn Subscribe>> = vec![Arc::new(TracingBridge)];
//! let sup = Supervisor::new(SupervisorConfig::default(), subs);
//! ```

use tracing::Level;

use crate::events::{Event, EventKind};
use crate::reasons::MAX_RETRIES_EXCEEDED;
use crate::subscribers::Subscribe;

/// Sends runtime events to [`tracing`] as structured events.
///
/// Level mapping:
/// - `ERROR`: task failed, actor dead, subscriber panicked.
/// - `WARN`: timeout, grace exceeded, subscriber overflow, add failed, controller rejected.
/// - `INFO`: lifecycle milestones (stopped, canceled, added, removed, shutdown, submitted, and `actor_exhausted` for every other reason).
/// - `DEBUG`: chatty events (starting, backoff, add/remove requests, slot transitions).
///
/// ## Also
///
/// - See [`Subscribe`] for the subscriber contract and queue/overflow semantics.
/// - See [`EventKind::as_label`] for the stable `event` field values.
#[cfg_attr(docsrs, doc(cfg(feature = "tracing")))]
#[derive(Default)]
pub struct TracingBridge;

/// Maps an event to a tracing level.
fn level_for(e: &Event) -> Level {
    match e.kind {
        EventKind::TaskFailed | EventKind::ActorDead | EventKind::SubscriberPanicked => {
            Level::ERROR
        }

        EventKind::TimeoutHit
        | EventKind::GraceExceeded
        | EventKind::SubscriberOverflow
        | EventKind::TaskAddFailed => Level::WARN,

        EventKind::ActorExhausted => {
            let gave_up = e
                .reason
                .as_deref()
                .is_some_and(|r| r.starts_with(MAX_RETRIES_EXCEEDED));
            if gave_up { Level::WARN } else { Level::INFO }
        }

        EventKind::TaskStopped
        | EventKind::TaskCanceled
        | EventKind::TaskAdded
        | EventKind::TaskRemoved
        | EventKind::ShutdownRequested
        | EventKind::AllStoppedWithinGrace => Level::INFO,

        EventKind::TaskStarting
        | EventKind::BackoffScheduled
        | EventKind::TaskAddRequested
        | EventKind::TaskRemoveRequested => Level::DEBUG,

        #[cfg(feature = "controller")]
        EventKind::ControllerRejected => Level::WARN,
        #[cfg(feature = "controller")]
        EventKind::ControllerSubmitted => Level::INFO,
        #[cfg(feature = "controller")]
        EventKind::ControllerSlotTransition => Level::DEBUG,
    }
}

impl Subscribe for TracingBridge {
    fn on_event(&self, e: &Event) {
        macro_rules! emit {
            ($level:expr) => {
                tracing::event!(
                    target: "taskvisor",
                    $level,
                    event = e.kind.as_label(),
                    seq = e.seq,
                    id = e.id.map(tracing::field::display),
                    task = e.task.as_deref(),
                    attempt = e.attempt.map(u64::from),
                    reason = e.reason.as_deref(),
                    delay_ms = e.delay_ms.map(u64::from),
                    timeout_ms = e.timeout_ms.map(u64::from),
                    duration_ms = e.duration_ms.map(u64::from),
                    exit_code = e.exit_code.map(i64::from),
                    backoff_source = e.backoff_source.map(|s| s.as_label()),
                )
            };
        }

        match level_for(e) {
            Level::ERROR => emit!(Level::ERROR),
            Level::WARN => emit!(Level::WARN),
            Level::INFO => emit!(Level::INFO),
            Level::DEBUG => emit!(Level::DEBUG),
            _ => emit!(Level::TRACE),
        }
    }

    fn name(&self) -> &str {
        "TracingBridge"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Event, EventKind};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::{Level, Metadata, span};

    type Captured = (Level, HashMap<String, String>);

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<Captured>>>);

    struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

    impl Visit for FieldVisitor<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
        }
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
        fn record_u64(&mut self, field: &Field, value: u64) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
        fn record_i64(&mut self, field: &Field, value: i64) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
    }

    impl tracing::Subscriber for Capture {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }
        fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
        fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut fields = HashMap::new();
            event.record(&mut FieldVisitor(&mut fields));
            self.0
                .lock()
                .unwrap()
                .push((*event.metadata().level(), fields));
        }
        fn enter(&self, _: &span::Id) {}
        fn exit(&self, _: &span::Id) {}
    }

    fn capture_one(e: &Event) -> (Level, HashMap<String, String>) {
        let cap = Capture::default();
        tracing::subscriber::with_default(cap.clone(), || {
            crate::subscribers::Subscribe::on_event(&TracingBridge, e);
        });
        let mut events = cap.0.lock().unwrap();
        assert_eq!(events.len(), 1, "exactly one tracing event expected");
        events.pop().unwrap()
    }

    #[test]
    fn task_failed_maps_to_error_with_structured_fields() {
        let e = Event::new(EventKind::TaskFailed)
            .with_task("worker")
            .with_reason("boom")
            .with_attempt(2);

        let (level, fields) = capture_one(&e);

        assert_eq!(level, Level::ERROR, "TaskFailed must map to ERROR");
        assert_eq!(fields.get("event").map(String::as_str), Some("task_failed"));
        assert_eq!(fields.get("task").map(String::as_str), Some("worker"));
        assert_eq!(fields.get("reason").map(String::as_str), Some("boom"));
        assert_eq!(fields.get("attempt").map(String::as_str), Some("2"));
    }

    #[test]
    fn levels_match_event_severity() {
        let cases = [
            (EventKind::TaskStopped, Level::INFO),
            (EventKind::TaskStarting, Level::DEBUG),
            (EventKind::TimeoutHit, Level::WARN),
            (EventKind::ActorDead, Level::ERROR),
            (EventKind::GraceExceeded, Level::WARN),
            (EventKind::BackoffScheduled, Level::DEBUG),
        ];
        for (kind, expected) in cases {
            let (level, _) = capture_one(&Event::new(kind));
            assert_eq!(level, expected, "wrong level for {kind:?}");
        }
    }

    #[test]
    fn actor_exhausted_level_depends_on_reason() {
        for (reason, expected) in [
            (Some("max_retries_exceeded(3/3): boom"), Level::WARN),
            (Some("policy_exhausted_success"), Level::INFO),
            (Some("task_returned_canceled"), Level::INFO),
            (None, Level::INFO),
        ] {
            let mut e = Event::new(EventKind::ActorExhausted).with_task("worker");
            if let Some(reason) = reason {
                e = e.with_reason(reason);
            }
            let (level, _) = capture_one(&e);
            assert_eq!(level, expected, "wrong level for reason {reason:?}");
        }
    }

    #[test]
    fn absent_optional_fields_are_skipped() {
        let (_, fields) = capture_one(&Event::new(EventKind::ShutdownRequested));

        assert_eq!(
            fields.get("event").map(String::as_str),
            Some("shutdown_requested")
        );
        assert!(fields.contains_key("seq"), "seq is always present");
        for absent in [
            "id",
            "task",
            "reason",
            "attempt",
            "delay_ms",
            "timeout_ms",
            "duration_ms",
            "exit_code",
            "backoff_source",
        ] {
            assert!(
                !fields.contains_key(absent),
                "unset optional field {absent:?} must not be recorded"
            );
        }
    }
}
