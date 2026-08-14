//! # Application and lifecycle events with `tracing`
//!
//! `TracingBridge` sends Taskvisor lifecycle events to the application's active `tracing` dispatcher.
//! The application still owns formatting, filtering, and output sinks.
//!
//! ```text
//! task code ───────────► target=example_service ──► tracing dispatcher
//! Taskvisor events ────► TracingBridge ───────────► target=taskvisor ───► tracing dispatcher
//! tracing dispatcher ──► filters and sinks
//! ```
//!
//! The default bridge omits free-form `Event::reason` text. Typed fields remain available.
//! Use `TracingBridge::with_reasons()` when diagnostic reason text must enter the pipeline.
//! This program uses the default bridge.
//!
//! The default filter shows both targets at `DEBUG` and above. Override it with `RUST_LOG`.
//!
//! For example, use `RUST_LOG=taskvisor=debug,example_service=info`.
//! Expect two failed attempts, two backoffs, a third successful attempt, and a final outcome.
//! The exact line format belongs to `tracing-subscriber`. The example exits after success.
//!
//! Run with `cargo run --example tracing --features tracing`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use taskvisor::TracingBridge;
use taskvisor::prelude::*;
use tracing_subscriber::EnvFilter;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Your service sets up tracing once, as usual.
    // Default filter: show application and Taskvisor events, DEBUG and up.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("taskvisor=debug,example_service=debug")),
        )
        .init();

    // A flaky task: fails twice, then succeeds.
    // Watch task work, retry failures, backoff, and the terminal outcome.
    let attempts = Arc::new(AtomicU32::new(0));
    let flaky: TaskRef = TaskFn::arc(move |_ctx| {
        let attempts = Arc::clone(&attempts);
        async move {
            let n = attempts.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::debug!(
                target: "example_service",
                operation = "catalog_sync",
                attempt = n,
                "requesting catalog batch"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
            if n <= 2 {
                return Err(TaskError::fail(format!("boom #{n}")));
            }
            tracing::info!(
                target: "example_service",
                operation = "catalog_sync",
                attempt = n,
                records = 128_u64,
                "catalog batch applied"
            );
            Ok(())
        }
    });

    let spec = TaskSpec::restartable("flaky-job", flaky)
        .with_backoff(BackoffPolicy::constant(Duration::from_millis(100)));

    // This default bridge omits Event::reason. Use with_reasons() to opt in.
    let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::new(TracingBridge)];
    let supervisor = Supervisor::new(SupervisorConfig::default(), subscribers);
    supervisor.run(vec![spec]).await?;

    Ok(())
}
