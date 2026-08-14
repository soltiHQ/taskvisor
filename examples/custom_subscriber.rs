//! # Custom event subscriber
//!
//! Implement `Subscribe` to consume typed lifecycle events without blocking their publishers.
//! This example counts attempt and backoff events with atomics in one synchronous callback.
//!
//! ```text
//! runtime publishers
//!       │
//!       ▼
//! shared bounded bus                   full: discard oldest, retain newest
//!       │ relay and fan-out
//!       ▼
//! one bounded subscriber lane          full: drop new event for this subscriber
//!       │ serial callback executor
//!       ▼
//! Subscribe::on_event
//! ```
//!
//! The normal delivery path has these two separate loss points.
//! The relay attempts an overflow diagnostic for bus loss.
//! After a full subscriber lane catches up, Taskvisor delivers one coalesced overflow callback if that lane remains active.
//! Loss in one lane does not alter another lane.
//! Shutdown may also discard queued events when the shared drain deadline expires.
//!
//! Keep `on_event` short.
//! Forward async or blocking work to an application-owned queue.
//! Events are for observation. Use `TaskWaiter` when correctness depends on a final result.
//!
//! Expect four starts, three failures, three backoffs, and one success.
//! The fourth attempt succeeds, subscriber shutdown drains, and the example exits.
//!
//! Run with `cargo run --example custom_subscriber`.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use taskvisor::prelude::*;

/// A simple metrics subscriber that counts lifecycle events.
struct Metrics {
    starts: AtomicU64,
    successes: AtomicU64,
    failures: AtomicU64,
    backoffs: AtomicU64,
}

impl Metrics {
    fn new() -> Self {
        Self {
            starts: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            backoffs: AtomicU64::new(0),
        }
    }

    fn report(&self) {
        println!();
        println!("--- Metrics ---");
        println!("  starts:   {}", self.starts.load(Ordering::Relaxed));
        println!("  successes: {}", self.successes.load(Ordering::Relaxed));
        println!("  failures: {}", self.failures.load(Ordering::Relaxed));
        println!("  backoffs: {}", self.backoffs.load(Ordering::Relaxed));
    }
}

impl Subscribe for Metrics {
    fn on_event(&self, event: &Event) {
        match event.kind {
            EventKind::AttemptStarting => {
                self.starts.fetch_add(1, Ordering::Relaxed);
            }
            EventKind::AttemptSucceeded => {
                self.successes.fetch_add(1, Ordering::Relaxed);
            }
            EventKind::AttemptFailed => {
                self.failures.fetch_add(1, Ordering::Relaxed);
            }
            EventKind::BackoffScheduled => {
                self.backoffs.fetch_add(1, Ordering::Relaxed);
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
    let flaky: TaskRef = TaskFn::arc(move |_ctx| {
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
    let spec = TaskSpec::restartable("flaky-job", flaky);

    let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::clone(&metrics) as _];
    let supervisor = Supervisor::new(SupervisorConfig::default(), subscribers);
    supervisor.run(vec![spec]).await?;

    metrics.report();
    Ok(())
}
