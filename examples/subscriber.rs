//! # Subscriber: Custom Event Handler
//!
//! Implements the `Subscribe` trait to collect task lifecycle counters.
//!
//! This example counts events with plain atomics to keep the focus on the trait.
//! For a real metrics integration see `metrics.rs` (Prometheus) and `tracing.rs`.
//
//! ## What this shows
//!
//! - **`on_event(&self, event: &Event)`** - your sync handler, run on Tokio's blocking pool.
//! - **`Subscribe` trait** - extension point for observability.
//! - **`queue_capacity()`** - per-subscriber buffer size (overflow → event dropped).
//! - **`name()`** - identifier for logs and overflow/panic events.
//!
//! ## How subscribers are wired
//!
//! ```text
//! Supervisor::new(config, vec![metrics])
//!                               ▼
//!                        SubscriberSet
//!                         ├── [mpsc queue] → worker → blocking pool → metrics.on_event()
//!                         └── [mpsc queue] → worker → blocking pool → other_sub.on_event()
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
//! cargo run --example subscriber
//! ```
//!
//! ## Next
//!
//! | Example                      | What it adds                                          |
//! |------------------------------|-------------------------------------------------------|
//! | [`metrics.rs`](metrics.rs)   | Real Prometheus counters keyed by `as_label()`        |
//! | [`tracing.rs`](tracing.rs)   | Forward events into the `tracing` ecosystem           |
//! | [`dynamic.rs`](dynamic.rs)   | `serve()` → `SupervisorHandle` for runtime management |

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

    fn queue_capacity(&self) -> usize {
        2048
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

    // restartable() already uses the default backoff (100ms, constant).
    let spec = TaskSpec::restartable(flaky);

    let subs: Vec<Arc<dyn Subscribe>> = vec![Arc::clone(&metrics) as _];
    let sup = Supervisor::new(SupervisorConfig::default(), subs);
    sup.run(vec![spec]).await?;

    metrics.report();
    Ok(())
}
