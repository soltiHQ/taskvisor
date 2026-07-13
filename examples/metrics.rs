//! # Metrics: Prometheus Counters from Lifecycle Events
//!
//! A real metrics integration: every supervisor event increments a Prometheus
//! counter, keyed by the event's stable label.
//!
//! ## The pattern
//!
//! - One `IntCounterVec` with labels `event` and `task`.
//! - `EventKind::as_label()` gives a stable, machine-readable label value
//!   (`task_failed`, `backoff_scheduled`, ...). No hand-written match.
//! - In a real service, serve the encoded text at `GET /metrics`.
//!   This example prints it at exit instead.
//!
//! ## Label cardinality
//!
//! The `task` label uses the task name.
//! Keep task names a bounded set (no per-request ids in names) to avoid
//! high-cardinality metrics.
//!
//! ## What this shows
//!
//! - `Subscribe` + `prometheus::IntCounterVec` - the whole bridge is ~15 lines.
//! - `EventKind::as_label()` as the metric label value.
//! - `TextEncoder` - rendering the standard exposition format.
//!
//! ## Run
//!
//! ```bash
//! cargo run --example metrics
//! ```
//!
//! ## Next
//!
//! | Example                          | What it adds                                 |
//! |----------------------------------|----------------------------------------------|
//! | [`tracing.rs`](tracing.rs)       | The same event stream in your logs           |
//! | [`subscriber.rs`](subscriber.rs) | The `Subscribe` trait itself, step by step   |

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
    fn on_event(&self, e: &Event) {
        let task = e.task.as_deref().unwrap_or("none");
        self.events
            .with_label_values(&[e.kind.as_label(), task])
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
        &["event", "task"],
    )?;
    registry.register(Box::new(events.clone()))?;

    // A flaky task: fails twice, then succeeds.
    let attempts = Arc::new(AtomicU32::new(0));
    let flaky: TaskRef = TaskFn::arc("flaky-job", move |_ctx| {
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

    let spec = TaskSpec::restartable(flaky)
        .with_backoff(BackoffPolicy::constant(Duration::from_millis(100)));

    let subs: Vec<Arc<dyn Subscribe>> = vec![Arc::new(PromSubscriber {
        events: events.clone(),
    })];
    let sup = Supervisor::new(SupervisorConfig::default(), subs);
    sup.run(vec![spec]).await?;

    // In a real service: serve this string at GET /metrics.
    let mut buf = Vec::new();
    TextEncoder::new().encode(&registry.gather(), &mut buf)?;
    println!("\n--- /metrics ---\n{}", String::from_utf8(buf)?);
    Ok(())
}
