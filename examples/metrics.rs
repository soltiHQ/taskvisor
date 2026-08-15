//! # Metrics from lifecycle events
//!
//! This example maps best-effort lifecycle events into one Prometheus counter family.
//! Stable enum labels provide bounded event and outcome categories.
//!
//! ```text
//! Event.kind         ──► event   ──┐
//! Event.outcome_kind ──► outcome ──┼──► taskvisor_events_total
//! Event.task         ──► subject ──┘
//! ```
//!
//! `Event::task` is a polymorphic subject. It usually contains a task name.
//! Diagnostics may store a subscriber, relay, or runtime component name in the same field.
//! Controller events store a slot name. The Prometheus label is therefore named `subject`.
//!
//! Keep every possible subject bounded and stable.
//! Do not use request IDs or user IDs as labels.
//! Diagnostic `reason` text is not a label because it is free-form and may have high cardinality.
//!
//! A service would expose the Prometheus registry on its metrics endpoint.
//! Expect two failed attempts, one success, printed Prometheus text, and a normal exit.
//!
//! Run with `cargo run --example metrics`.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use prometheus::{Encoder, IntCounterVec, Opts, Registry, TextEncoder};
use taskvisor::prelude::*;

/// Bridges supervisor events into a Prometheus counter family.
struct PromSubscriber {
    events: IntCounterVec,
}

impl Subscribe for PromSubscriber {
    fn on_event(&self, event: &Event) {
        let subject = event.task.as_deref().unwrap_or("none");
        let outcome = event
            .outcome_kind
            .map(TaskOutcomeKind::as_label)
            .unwrap_or("none");
        self.events
            .with_label_values(&[event.kind.as_label(), outcome, subject])
            .inc();
    }

    fn name(&self) -> &str {
        "prometheus"
    }

    fn queue_capacity(&self) -> NonZeroUsize {
        NonZeroUsize::new(2048).unwrap()
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let registry = Registry::new();
    let events = IntCounterVec::new(
        Opts::new("taskvisor_events_total", "Supervisor lifecycle events"),
        &["event", "outcome", "subject"],
    )?;
    registry.register(Box::new(events.clone()))?;

    // A flaky task: fails twice, then succeeds.
    let attempts = Arc::new(AtomicU32::new(0));
    let flaky: TaskRef = TaskFn::arc(move |_ctx| {
        let attempts = Arc::clone(&attempts);
        async move {
            let n = attempts.fetch_add(1, Ordering::Relaxed) + 1;
            tokio::time::sleep(Duration::from_millis(50)).await;
            if n <= 2 {
                return Err(TaskError::fail(format!("boom #{n}")));
            }
            Ok(())
        }
    });

    let spec = TaskSpec::restartable("flaky-job", flaky)
        .with_backoff(BackoffPolicy::constant(Duration::from_millis(100)));

    let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::new(PromSubscriber {
        events: events.clone(),
    })];
    let supervisor = Supervisor::new(SupervisorConfig::default(), subscribers);
    supervisor.run(vec![spec]).await?;

    // In a real service: serve this string at GET /metrics.
    let mut buf = Vec::new();
    TextEncoder::new().encode(&registry.gather(), &mut buf)?;
    println!("\n--- /metrics ---\n{}", String::from_utf8(buf)?);
    Ok(())
}
