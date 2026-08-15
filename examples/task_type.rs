//! # Implement a named task type
//!
//! `TaskFn` adapts an async closure.
//! Implement `Task` when work owns reusable dependencies or state that must survive across attempts.
//! The task object persists; every `spawn` call must return a fresh future for one attempt.
//!
//! ```text
//! Task object
//!      ├── spawn(ctx) ──► attempt 1 future ──► retryable failure
//!      └── spawn(ctx) ──► attempt 2 future ──► success
//! ```
//!
//! `spawn` runs synchronously before the attempt timeout starts.
//! Keep it short and move work into the returned future.
//! A shared `TaskRef` may be registered more than once, which can call `spawn` concurrently;
//! shared state must be thread-safe.
//!
//! Run with `cargo run --example task_type`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use taskvisor::prelude::*;

struct EndpointProbe {
    endpoint: Arc<str>,
    attempts: AtomicU32,
}

impl Task for EndpointProbe {
    fn spawn(&self, ctx: TaskContext) -> BoxTaskFuture {
        let endpoint = Arc::clone(&self.endpoint);
        let attempt = self.attempts.fetch_add(1, Ordering::Relaxed) + 1;

        Box::pin(async move {
            println!("[probe] {endpoint}, attempt #{attempt}");
            ctx.run_until_cancelled(tokio::time::sleep(Duration::from_millis(100)))
                .await?;
            if attempt == 1 {
                Err(TaskError::fail("endpoint not ready"))
            } else {
                Ok(())
            }
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let task: TaskRef = Arc::new(EndpointProbe {
        endpoint: Arc::from("https://service.internal/health"),
        attempts: AtomicU32::new(0),
    });
    let spec = TaskSpec::restartable("endpoint-probe", task)
        .with_backoff(BackoffPolicy::constant(Duration::from_millis(100)));

    let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    supervisor.run(vec![spec]).await?;
    Ok(())
}
