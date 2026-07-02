//! # Tracing: Supervisor Events in Your Log Pipeline
//!
//! Forwards every lifecycle event into the [`tracing`](https://docs.rs/tracing) ecosystem
//! with the built-in `TracingBridge` subscriber.
//!
//! This is the production path for logging.
//! Your service already formats and ships `tracing` output.
//! The bridge makes supervisor events part of that stream: same format, same filters, same sinks.
//!
//! ## What this shows
//!
//! - `TracingBridge` - one line to wire taskvisor into `tracing`. Requires `--features tracing`.
//! - Level mapping: failures are `ERROR`, timeouts and overflows are `WARN`,
//!   lifecycle milestones are `INFO`, chatty events (attempts, backoff) are `DEBUG`.
//! - Structured fields: `event` (stable label), `task`, `attempt`, `reason`, `delay_ms`, ...
//! - Filtering with `RUST_LOG`: try `RUST_LOG=taskvisor=warn` to see only problems.
//!
//! ## Enabling the feature
//!
//! ```toml
//! [dependencies]
//! taskvisor = { version = "...", features = ["tracing"] }
//! ```
//!
//! ## Run
//!
//! ```bash
//! cargo run --example tracing --features tracing
//! # Only warnings and errors:
//! RUST_LOG=taskvisor=warn cargo run --example tracing --features tracing
//! ```
//!
//! ## Next
//!
//! | Example                          | What it adds                                   |
//! |----------------------------------|------------------------------------------------|
//! | [`metrics.rs`](metrics.rs)       | Prometheus counters from the same event stream |
//! | [`subscriber.rs`](subscriber.rs) | Writing your own `Subscribe` implementation    |

#[cfg(not(feature = "tracing"))]
compile_error!(
    "This example requires the `tracing` feature: cargo run --example tracing --features tracing"
);

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use taskvisor::TracingBridge;
use taskvisor::prelude::*;
use tracing_subscriber::EnvFilter;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Your service sets up tracing once, as usual.
    // Default filter: show everything taskvisor emits, DEBUG and up.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("taskvisor=debug")),
        )
        .init();

    // A flaky task: fails twice, then succeeds.
    // Watch the recovery as ERROR -> DEBUG(backoff) -> DEBUG(start) -> INFO lines.
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

    // One line: supervisor events flow into your log pipeline.
    let subs: Vec<Arc<dyn Subscribe>> = vec![Arc::new(TracingBridge)];
    let sup = Supervisor::new(SupervisorConfig::default(), subs);
    sup.run(vec![spec]).await?;

    Ok(())
}
