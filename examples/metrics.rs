//! # Metrics from lifecycle events
//!
//! This example implements `Subscribe` and maps each event to a Prometheus
//! counter. `EventKind::as_label` provides stable, machine-readable values such
//! as `task_failed` and `backoff_scheduled`.
//!
//! A real service would expose the registry on its metrics endpoint. This
//! example prints the Prometheus text format when it exits.
//!
//! The `task` label uses task names. Keep these names bounded and stable. Do not
//! put request IDs, user IDs, or other unbounded values in metric labels.
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
