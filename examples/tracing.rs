//! # Send lifecycle events to `tracing`
//!
//! The optional `TracingBridge` sends structured supervisor events to the application's existing `tracing` pipeline.
//! The normal formatter, filters, and output sinks continue to apply.
//!
//! The example shows event levels, stable event labels, task fields, attempt numbers, reasons, and delays.
//! Set `RUST_LOG=taskvisor=warn` to keep only warnings and errors.
//!
//! Run with `cargo run --example tracing --features tracing`.

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
