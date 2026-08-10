//! # Application and lifecycle events with `tracing`
//!
//! The optional `TracingBridge` sends structured supervisor events to the application's existing `tracing` pipeline.
//! The normal formatter, filters, and output sinks continue to apply.
//!
//! The task emits application events with target `example_service`.
//! `TracingBridge` emits lifecycle events with target `taskvisor`.
//! Set `RUST_LOG=taskvisor=warn,example_service=info` to filter the two targets independently.
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
    let flaky: TaskRef = TaskFn::arc("flaky-job", move |_ctx| {
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

    let spec = TaskSpec::restartable(flaky)
        .with_backoff(BackoffPolicy::constant(Duration::from_millis(100)));

    // One line: supervisor events flow into the same tracing pipeline.
    let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::new(TracingBridge)];
    let supervisor = Supervisor::new(SupervisorConfig::default(), subscribers);
    supervisor.run(vec![spec]).await?;

    Ok(())
}
