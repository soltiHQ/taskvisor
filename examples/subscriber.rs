//! # Custom event subscriber
//!
//! This example implements `Subscribe` and counts lifecycle events with atomics.
//! Each subscriber has its own bounded queue:
//!
//! ```text
//! supervisor events
//!       │
//!       ├──► bounded queue ──► worker ──► blocking pool ──► on_event(metrics)
//!       └──► bounded queue ──► worker ──► blocking pool ──► on_event(other)
//! ```
//!
//! A slow subscriber does not block event producers, but its queue can overflow
//! and lose events. Keep `on_event` bounded and use outcomes for decisions that
//! must not depend on best-effort delivery.
//!
//! Run with `cargo run --example subscriber`.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use taskvisor::prelude::*;

/// A simple metrics subscriber that counts lifecycle events.
struct Metrics {
    starts: AtomicU64,
    stops: AtomicU64,
    failures: AtomicU64,
    retries: AtomicU64,
}

impl Metrics {
    fn new() -> Self {
        Self {
            starts: AtomicU64::new(0),
            stops: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            retries: AtomicU64::new(0),
        }
    }

    fn report(&self) {
        println!();
        println!("--- Metrics ---");
        println!("  starts:   {}", self.starts.load(Ordering::Relaxed));
        println!("  stops:    {}", self.stops.load(Ordering::Relaxed));
        println!("  failures: {}", self.failures.load(Ordering::Relaxed));
        println!("  retries:  {}", self.retries.load(Ordering::Relaxed));
    }
}

impl Subscribe for Metrics {
    fn on_event(&self, ev: &Event) {
        match ev.kind {
            EventKind::TaskStarting => {
                self.starts.fetch_add(1, Ordering::Relaxed);
            }
            EventKind::TaskStopped => {
                self.stops.fetch_add(1, Ordering::Relaxed);
            }
            EventKind::TaskFailed => {
                self.failures.fetch_add(1, Ordering::Relaxed);
            }
            EventKind::BackoffScheduled => {
                self.retries.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    fn name(&self) -> &str {
        "metrics"
    }

    fn queue_capacity(&self) -> NonZeroUsize {
        NonZeroUsize::new(2048).unwrap()
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let metrics = Arc::new(Metrics::new());

    // A "flaky" task that fails 3 times then succeeds.
    let counter = Arc::new(AtomicU32::new(0));
    let flaky: TaskRef = TaskFn::arc("flaky-job", move |_ctx| {
        let counter = Arc::clone(&counter);
        async move {
            let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
            tokio::time::sleep(Duration::from_millis(50)).await;

            if n <= 3 {
                println!("[flaky-job] attempt #{n}: fail");
                Err(TaskError::fail(format!("attempt #{n}")))
            } else {
                println!("[flaky-job] attempt #{n}: success!");
                Ok(())
            }
        }
    });

    // restartable() uses exponential backoff from 200ms to 30s with equal jitter.
    let spec = TaskSpec::restartable(flaky);

    let subs: Vec<Arc<dyn Subscribe>> = vec![Arc::clone(&metrics) as _];
    let sup = Supervisor::new(SupervisorConfig::default(), subs);
    sup.run(vec![spec]).await?;

    metrics.report();
    Ok(())
}
