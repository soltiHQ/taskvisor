//! # Readable lifecycle logs
//!
//! Enable `logging` and add `LogWriter` when a demo or small tool needs immediate
//! lifecycle output without configuring a tracing stack.
//!
//! ```text
//! runtime events ──► bounded event bus ──► subscriber queue ──► LogWriter ──► stdout
//! ```
//!
//! Delivery is best-effort.
//! Use a custom subscriber or `TracingBridge` for structured output.
//! This task fails once, waits for backoff, then succeeds to produce a useful event sequence.
//!
//! Run with `cargo run --example logging --features logging`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use taskvisor::prelude::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let attempts = Arc::new(AtomicU32::new(0));
    let task: TaskRef = TaskFn::arc(move |_ctx| {
        let attempts = Arc::clone(&attempts);
        async move {
            let attempt = attempts.fetch_add(1, Ordering::Relaxed) + 1;
            if attempt == 1 {
                Err(TaskError::fail("temporary failure"))
            } else {
                Ok(())
            }
        }
    });

    let spec = TaskSpec::restartable("logged-job", task)
        .with_backoff(BackoffPolicy::constant(Duration::from_millis(100)));
    let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::new(LogWriter)];
    let supervisor = Supervisor::new(SupervisorConfig::default(), subscribers);

    supervisor.run(vec![spec]).await?;
    Ok(())
}
