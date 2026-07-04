//! # Worker: Long-Running Task with Graceful Shutdown
//!
//! A background worker that ticks every 500ms and exits cleanly on Ctrl+C.
//! This is the canonical pattern for any task that runs indefinitely.
//!
//! ## What this shows
//!
//! - **Cooperative cancellation** via `ctx.run_until_cancelled(...)`.
//!   Every long-running task **MUST** observe cancellation via its `TaskContext`.
//!   Without it, the task would keep running inside `sleep()` and ignore the shutdown signal until the grace period expires.
//! - `TaskSpec::restartable(task)` uses `RestartPolicy::OnFailure` by default:
//!   the worker restarts only if it fails, not on clean exits.
//!
//! ## Why observing cancellation matters
//!
//! Tokio **does not kill futures**: it cancels them cooperatively.
//! When the supervisor shuts down, it cancels the task's `TaskContext`.
//! If your task is awaiting `sleep(10s)`, it won't notice until the sleep finishes.
//! Wrapping the await lets the task react immediately.
//!
//! Three patterns, from most to least common:
//! ```text
//! // Pattern 1: run_until_cancelled (recommended)
//! // Resolves to Err(TaskError::Canceled) on shutdown; `?` exits the loop cleanly.
//! ctx.run_until_cancelled(do_work()).await?;
//!
//! // Pattern 2: select! (manual control over the cancel branch)
//! tokio::select! {
//!     _ = ctx.cancelled() => return Err(TaskError::Canceled),
//!     _ = do_work()       => { ... }
//! }
//!
//! // Pattern 3: check (ok for short, non-blocking tasks)
//! if ctx.is_cancelled() { return Err(TaskError::Canceled); }
//! do_quick_work().await;
//! ```
//!
//! Returning `TaskError::Canceled` is a clean stop, not a failure.
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
//! cargo run --example worker
//! # Press Ctrl+C to stop
//! ```
//!
//! ## Next
//!
//! | Example                      | What it adds                          |
//! |------------------------------|---------------------------------------|
//! | [`periodic.rs`](periodic.rs) | Task that repeats on a fixed interval |
//! | [`multiple.rs`](multiple.rs) | Several tasks with different policies |
//!

use std::time::Duration;

use taskvisor::prelude::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let worker: TaskRef = TaskFn::arc("ticker", |ctx| async move {
        let mut tick = 0u64;
        loop {
            match ctx
                .run_until_cancelled(tokio::time::sleep(Duration::from_millis(500)))
                .await
            {
                Ok(()) => {
                    tick += 1;
                    println!("[ticker] tick #{tick}");
                }
                Err(canceled) => {
                    println!("[ticker] shutting down after {tick} ticks");
                    return Err(canceled); // clean stop, not a failure
                }
            }
        }
    });

    let spec = TaskSpec::restartable(worker);

    let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
    sup.run(vec![spec]).await?;

    Ok(())
}
