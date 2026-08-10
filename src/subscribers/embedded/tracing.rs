//! # Bridge to `tracing`
//!
//! [`TracingBridge`] converts every event it receives into one structured [`tracing`] event.
//!
//! Each tracing event uses target `taskvisor` and contains:
//! - a level based on the event severity (see [`TracingBridge`]),
//! - structured fields: `event` (the stable label), `event_seq`, `event_unix_ms`, and these optional
//!   payload fields when set: `task_name`, `taskvisor_id`, `subscriber`, `component`, `slot`, `attempt`,
//!   `outcome_kind`, `rejection_kind`, `delay_ms`, `timeout_ms`, `duration_ms`, `dropped`, `exit_code`,
//!   `backoff_source`.
//!
//! Unset optional fields are not recorded.
//! Free-form [`Event::reason`] text is omitted by default.
//! Use [`TracingBridge::with_reasons`] to include it.
//!
//! ## Example
//! ```rust,no_run
//! use std::sync::Arc;
//! use taskvisor::{Subscribe, Supervisor, SupervisorConfig, TracingBridge};
//!
//! let subs: Vec<Arc<dyn Subscribe>> = vec![Arc::new(TracingBridge)];
//! let sup = Supervisor::new(SupervisorConfig::default(), subs);
//! ```

use std::{borrow::Cow, time::UNIX_EPOCH};
use tracing::Level;

use crate::TaskOutcomeKind;
use crate::events::{Event, EventKind, RejectionKind};
use crate::subscribers::Subscribe;

const MAX_TEXT_CHARS: usize = 4096;

fn bounded_text(value: &str) -> Cow<'_, str> {
    let mut chars = value.char_indices();
    let Some((end, _)) = chars.nth(MAX_TEXT_CHARS) else {
        return Cow::Borrowed(value);
    };
    let mut value = value[..end].to_owned();
    value.push_str("…[truncated]");
    Cow::Owned(value)
}

/// Sends runtime events to [`tracing`] as structured events.
///
/// Level mapping:
/// - `ERROR`: runtime failures, subscriber panics, and fatal or panicked final outcomes.
/// - `WARN`:  failed or force-aborted final outcomes, grace exceeded, overflow, and admission failures.
/// - `INFO`:  completed or canceled final outcomes and shutdown milestones.
/// - `DEBUG`: failed or timed-out attempts, backoff, registration, removal, and expected rejections.
/// - `TRACE`: attempt transitions, management requests, and controller slot transitions.
///
/// Free-form reasons are omitted by default.
/// Use [`Self::with_reasons`] to include them.
///
/// ## Also
///
/// - See [`Subscribe`] for the subscriber contract and queue/overflow semantics.
/// - See [`EventKind::as_label`] for the stable `event` field values.
#[cfg_attr(docsrs, doc(cfg(feature = "tracing")))]
#[derive(Default)]
pub struct TracingBridge;

impl TracingBridge {
    /// Creates a bridge that includes free-form [`Event::reason`] text.
    ///
    /// Task code and runtime errors can provide the reason text.
    /// The application decides whether that text is allowed at its log destination.
    /// Free-form values are truncated after 4096 characters.
    #[must_use]
    pub const fn with_reasons() -> TracingBridgeWithReasons {
        TracingBridgeWithReasons
    }
}

/// A [`TracingBridge`] that includes free-form [`Event::reason`] text.
///
/// Create it with [`TracingBridge::with_reasons`].
#[cfg_attr(docsrs, doc(cfg(feature = "tracing")))]
#[derive(Default)]
pub struct TracingBridgeWithReasons;

fn rejection_level(kind: Option<RejectionKind>) -> Level {
    match kind {
        Some(RejectionKind::AdmissionFailed) | None => Level::WARN,
        Some(_) => Level::DEBUG,
    }
}

/// Maps an event to a tracing level.
fn level_for(e: &Event) -> Level {
    match e.kind {
        EventKind::SubscriberPanicked | EventKind::RuntimeFailure => Level::ERROR,

        EventKind::GraceExceeded | EventKind::SubscriberOverflow => Level::WARN,

        EventKind::TaskFinished => match e.outcome_kind {
            Some(TaskOutcomeKind::Fatal | TaskOutcomeKind::Panicked) => Level::ERROR,
            Some(TaskOutcomeKind::Failed | TaskOutcomeKind::ForceAborted) | None => Level::WARN,
            Some(TaskOutcomeKind::Rejected) => rejection_level(e.rejection_kind),
            Some(TaskOutcomeKind::Completed | TaskOutcomeKind::Canceled) => Level::INFO,
        },

        EventKind::ShutdownRequested | EventKind::AllStoppedWithinGrace => Level::INFO,

        EventKind::AttemptFailed
        | EventKind::AttemptTimedOut
        | EventKind::BackoffScheduled
        | EventKind::TaskAdded
        | EventKind::TaskRemoved => Level::DEBUG,

        EventKind::TaskAddFailed => rejection_level(e.rejection_kind),

        EventKind::AttemptStarting
        | EventKind::AttemptSucceeded
        | EventKind::AttemptCanceled
        | EventKind::TaskAddRequested
        | EventKind::TaskRemoveRequested => Level::TRACE,

        #[cfg(feature = "controller")]
        EventKind::ControllerRejected => rejection_level(e.rejection_kind),
        #[cfg(feature = "controller")]
        EventKind::ControllerSubmitted => Level::DEBUG,
        #[cfg(feature = "controller")]
        EventKind::ControllerSlotTransition => Level::TRACE,
    }
}

fn semantic_names(e: &Event) -> (Option<&str>, Option<&str>, Option<&str>, Option<&str>) {
    let value = e.task.as_deref();
    match e.kind {
        EventKind::SubscriberPanicked | EventKind::SubscriberOverflow => (None, value, None, None),
        EventKind::RuntimeFailure => (None, None, value, None),
        #[cfg(feature = "controller")]
        EventKind::ControllerRejected
        | EventKind::ControllerSubmitted
        | EventKind::ControllerSlotTransition => (None, None, None, value),
        _ => (value, None, None, None),
    }
}

fn event_unix_ms(e: &Event) -> Option<u64> {
    e.at.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
}

fn emit_event(e: &Event, include_reason: bool) {
    let (task_name, subscriber, component, slot) = semantic_names(e);
    let task_name = task_name.map(bounded_text);
    let subscriber = subscriber.map(bounded_text);
    let component = component.map(bounded_text);
    let slot = slot.map(bounded_text);
    let reason = include_reason
        .then_some(e.reason.as_deref())
        .flatten()
        .map(bounded_text);

    macro_rules! emit {
        ($level:expr) => {
            tracing::event!(
                target: "taskvisor",
                $level,
                event = e.kind.as_label(),
                event_seq = e.seq,
                event_unix_ms = event_unix_ms(e),
                taskvisor_id = e.id.map(|id| id.get()),
                task_name = task_name.as_deref(),
                subscriber = subscriber.as_deref(),
                component = component.as_deref(),
                slot = slot.as_deref(),
                attempt = e.attempt.map(u64::from),
                reason = reason.as_deref(),
                delay_ms = e.delay_ms.map(u64::from),
                timeout_ms = e.timeout_ms.map(u64::from),
                duration_ms = e.duration_ms.map(u64::from),
                dropped = e.dropped,
                exit_code = e.exit_code.map(i64::from),
                backoff_source = e.backoff_source.map(|source| source.as_label()),
                rejection_kind = e.rejection_kind.map(|kind| kind.as_label()),
                outcome_kind = e.outcome_kind.map(TaskOutcomeKind::as_label),
                "taskvisor event"
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

impl Subscribe for TracingBridge {
    fn on_event(&self, e: &Event) {
        emit_event(e, false);
    }

    fn name(&self) -> &str {
        "TracingBridge"
    }
}

impl Subscribe for TracingBridgeWithReasons {
    fn on_event(&self, e: &Event) {
        emit_event(e, true);
    }

    fn name(&self) -> &str {
        "TracingBridgeWithReasons"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Event, EventKind};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, UNIX_EPOCH};
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

    fn capture_one_with(subscriber: &dyn Subscribe, e: &Event) -> (Level, HashMap<String, String>) {
        let cap = Capture::default();
        tracing::subscriber::with_default(cap.clone(), || {
            subscriber.on_event(e);
        });
        let mut events = cap.0.lock().unwrap();
        assert_eq!(events.len(), 1, "exactly one tracing event expected");
        events.pop().unwrap()
    }

    fn capture_one(e: &Event) -> (Level, HashMap<String, String>) {
        capture_one_with(&TracingBridge, e)
    }

    #[test]
    fn attempt_failed_maps_to_debug_with_canonical_fields() {
        let mut e = Event::new(EventKind::AttemptFailed)
            .with_task("worker")
            .with_id(crate::TaskId::next())
            .with_reason("boom")
            .with_attempt(2);
        e.at = UNIX_EPOCH + Duration::from_millis(42);
        let id = e.id.unwrap().get().to_string();

        let (level, fields) = capture_one(&e);

        assert_eq!(level, Level::DEBUG, "retry attempts must not be errors");
        assert_eq!(
            fields.get("event").map(String::as_str),
            Some("attempt_failed")
        );
        assert_eq!(fields.get("task_name").map(String::as_str), Some("worker"));
        assert_eq!(
            fields.get("taskvisor_id").map(String::as_str),
            Some(id.as_str())
        );
        assert_eq!(fields.get("event_unix_ms").map(String::as_str), Some("42"));
        assert_eq!(
            fields.get("message").map(String::as_str),
            Some("taskvisor event")
        );
        assert!(!fields.contains_key("reason"));
        for legacy in ["seq", "id", "task"] {
            assert!(!fields.contains_key(legacy));
        }
        assert_eq!(fields.get("attempt").map(String::as_str), Some("2"));
    }

    #[test]
    fn levels_match_event_severity() {
        let cases = [
            (EventKind::AttemptSucceeded, Level::TRACE),
            (EventKind::AttemptStarting, Level::TRACE),
            (EventKind::AttemptTimedOut, Level::DEBUG),
            (EventKind::GraceExceeded, Level::WARN),
            (EventKind::BackoffScheduled, Level::DEBUG),
            (EventKind::TaskAdded, Level::DEBUG),
            (EventKind::ShutdownRequested, Level::INFO),
            (EventKind::SubscriberPanicked, Level::ERROR),
        ];
        for (kind, expected) in cases {
            let (level, _) = capture_one(&Event::new(kind));
            assert_eq!(level, expected, "wrong level for {kind:?}");
        }
    }

    #[test]
    fn task_finished_level_and_field_depend_on_outcome_kind() {
        for (outcome_kind, expected) in [
            (TaskOutcomeKind::Completed, Level::INFO),
            (TaskOutcomeKind::Canceled, Level::INFO),
            (TaskOutcomeKind::Failed, Level::WARN),
            (TaskOutcomeKind::ForceAborted, Level::WARN),
            (TaskOutcomeKind::Rejected, Level::WARN),
            (TaskOutcomeKind::Fatal, Level::ERROR),
            (TaskOutcomeKind::Panicked, Level::ERROR),
        ] {
            let e = Event::new(EventKind::TaskFinished)
                .with_task("worker")
                .with_outcome_kind(outcome_kind)
                .with_reason("free-form diagnostic text");
            let (level, _) = capture_one(&e);
            assert_eq!(level, expected, "wrong level for {outcome_kind:?}");

            let (_, fields) = capture_one(&e);
            assert_eq!(
                fields.get("outcome_kind").map(String::as_str),
                Some(outcome_kind.as_label())
            );
        }
    }

    #[test]
    fn expected_rejections_are_debug_and_admission_failures_are_warn() {
        for (kind, expected) in [
            (RejectionKind::AlreadyExists, Level::DEBUG),
            (RejectionKind::QueueFull, Level::DEBUG),
            (RejectionKind::AdmissionFailed, Level::WARN),
        ] {
            let event = Event::new(EventKind::TaskAddFailed).with_rejection_kind(kind);
            let (level, _) = capture_one(&event);
            assert_eq!(level, expected, "wrong level for {kind:?}");
        }
    }

    #[test]
    fn semantic_name_fields_do_not_overload_task() {
        let cases = [
            (EventKind::AttemptStarting, "task_name"),
            (EventKind::SubscriberOverflow, "subscriber"),
            (EventKind::RuntimeFailure, "component"),
        ];
        for (kind, expected_field) in cases {
            let (_, fields) = capture_one(&Event::new(kind).with_task("value"));
            assert_eq!(
                fields.get(expected_field).map(String::as_str),
                Some("value")
            );
            assert!(!fields.contains_key("task"));
        }

        let (_, overflow_fields) =
            capture_one(&Event::subscriber_overflow("subscriber", "full").with_dropped(42));
        assert_eq!(
            overflow_fields.get("dropped").map(String::as_str),
            Some("42")
        );

        #[cfg(feature = "controller")]
        {
            let (_, fields) =
                capture_one(&Event::new(EventKind::ControllerSubmitted).with_task("slot-a"));
            assert_eq!(fields.get("slot").map(String::as_str), Some("slot-a"));
            assert!(!fields.contains_key("task_name"));
        }
    }

    #[test]
    fn reasons_require_explicit_opt_in() {
        let event = Event::new(EventKind::AttemptFailed).with_reason("diagnostic detail");

        let (_, redacted) = capture_one(&event);
        assert!(!redacted.contains_key("reason"));

        let (_, verbose) = capture_one_with(&TracingBridge::with_reasons(), &event);
        assert_eq!(
            verbose.get("reason").map(String::as_str),
            Some("diagnostic detail")
        );

        let long = "x".repeat(MAX_TEXT_CHARS + 1);
        let event = Event::new(EventKind::AttemptFailed).with_reason(long);
        let (_, verbose) = capture_one_with(&TracingBridge::with_reasons(), &event);
        assert!(
            verbose
                .get("reason")
                .is_some_and(|reason| reason.ends_with("…[truncated]"))
        );
    }

    #[test]
    fn absent_optional_fields_are_skipped() {
        let (_, fields) = capture_one(&Event::new(EventKind::ShutdownRequested));

        assert_eq!(
            fields.get("event").map(String::as_str),
            Some("shutdown_requested")
        );
        assert!(
            fields.contains_key("event_seq"),
            "event_seq is always present"
        );
        assert!(
            fields.contains_key("event_unix_ms"),
            "runtime events have a wall-clock timestamp"
        );
        for absent in [
            "taskvisor_id",
            "task_name",
            "subscriber",
            "component",
            "slot",
            "reason",
            "attempt",
            "delay_ms",
            "timeout_ms",
            "duration_ms",
            "exit_code",
            "backoff_source",
            "rejection_kind",
            "outcome_kind",
        ] {
            assert!(
                !fields.contains_key(absent),
                "unset optional field {absent:?} must not be recorded"
            );
        }
    }
}
