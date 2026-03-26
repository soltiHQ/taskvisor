//! # Metrics (Custom Event Subscriber)
//!
//! Implements the `Subscribe` trait to collect task lifecycle counters.
//
//! ## What this shows
//!
//! - **`Subscribe` trait** - extension point for observability.
//! - **`queue_capacity()`** - per-subscriber buffer size (overflow → event dropped).
//! - **`on_event(&self, event: &Event)`** - your handler, called synchronously.
//! - **`name()`** - identifier for logs and overflow/panic events.
//!
//! ## How subscribers are wired
//!
//! ```text
//! Supervisor::new(config, vec![metrics])
//!                               ▼
//!                        SubscriberSet
//!                         ├── [mpsc queue] → worker → metrics.on_event()
//!                         └── [mpsc queue] → worker → other_sub.on_event()
//! ```
//!
//! ## Runtime flavor
//!
//! We use `current_thread` here because a single-threaded runtime is enough for examples and tests.
//!
//! *It can be used with `#[tokio::main]` (defaults to multi-thread): taskvisor works with both.*
//!
//! ## Run
//!
//! ```bash
//! cargo run --example metrics
//! ```
//!
//! ## Next
//!
//! | Example                      | What it adds                                          |
//! |------------------------------|-------------------------------------------------------|
//! | [`dynamic.rs`](dynamic.rs)   | `serve()` → `SupervisorHandle` for runtime management |
//! | [`pipeline.rs`](pipeline.rs) | Admission control with the `controller` feature       |

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

    fn name(&self) -> &'static str {
        "metrics"
    }

    fn queue_capacity(&self) -> usize {
        2048
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let metrics = Arc::new(Metrics::new());

    // A "flaky" task that fails 3 times then succeeds.
    let counter = Arc::new(AtomicU32::new(0));
    let flaky: TaskRef = TaskFn::arc("flaky-job", move |_ctx: CancellationToken| {
        let counter = Arc::clone(&counter);
        async move {
            let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
            tokio::time::sleep(Duration::from_millis(50)).await;

            if n <= 3 {
                println!("[flaky-job] attempt #{n} — fail");
                Err(TaskError::Fail {
                    reason: format!("attempt #{n}"),
                })
            } else {
                println!("[flaky-job] attempt #{n} — success!");
                Ok(())
            }
        }
    });

    let spec = TaskSpec::restartable(flaky).with_backoff(BackoffPolicy {
        first: Duration::from_millis(100),
        factor: 1.0,
        ..BackoffPolicy::default()
    });

    let subs: Vec<Arc<dyn Subscribe>> = vec![Arc::clone(&metrics) as _];
    let sup = Supervisor::new(SupervisorConfig::default(), subs);
    sup.run(vec![spec]).await?;

    metrics.report();
    Ok(())
}
