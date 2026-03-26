//! # Worker: Long-Running Task with Graceful Shutdown
//!
//! A background worker that ticks every 500ms and exits cleanly on Ctrl+C.
//! This is the canonical pattern for any task that runs indefinitely.
//!
//! ## What this shows
//!
//! - **Cooperative cancellation** via `tokio::select!` + `ctx.cancelled()`.
//!   Every long-running task **MUST** observe its `CancellationToken`.
//!   Without it, the task would keep running inside `sleep()` and ignore the shutdown signal until the grace period expires.
//! - `TaskSpec::restartable(task)` uses `RestartPolicy::OnFailure` by default:
//!   the worker restarts only if it returns `Err`, not on `Ok`.
//!
//! ## Why `ctx.cancelled()` matters
//!
//! Tokio **does NOT kill futures**: it cancels them cooperatively.
//! When the supervisor shuts down, it cancels the task's `CancellationToken`.
//! If your task is awaiting `sleep(10s)`, it won't notice until the sleep finishes.
//! `tokio::select!` with `ctx.cancelled()` lets the task react immediately.
//!
//! Two equivalent patterns:
//! ```text
//! // Pattern 1: select! (recommended for loops / long waits)
//! tokio::select! {
//!     _ = ctx.cancelled() => return Ok(()),
//!     _ = do_work()       => { ... }
//! }
//!
//! // Pattern 2: check (ok for short, non-blocking tasks)
//! if ctx.is_cancelled() { return Ok(()); }
//! do_quick_work().await;
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
//! cargo run --example worker
//! # Press Ctrl+C to stop
//! ```
//!
//! ## Next
//!
//! | Example                      | What it adds                          |
//! |------------------------------|---------------------------------------|
//! | [`periodic.rs`](periodic.rs) | Task that repeats on a schedule       |
//! | [`multiple`](multiple)       | Several tasks with different policies |
//!

use std::time::Duration;

use taskvisor::prelude::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let worker: TaskRef = TaskFn::arc("ticker", |ctx: CancellationToken| async move {
        let mut tick = 0u64;
        loop {
            tokio::select! {
                _ = ctx.cancelled() => {
                    println!("[ticker] shutting down after {tick} ticks");
                    return Ok(());
                }
                _ = tokio::time::sleep(Duration::from_millis(500)) => {
                    tick += 1;
                    println!("[ticker] tick #{tick}");
                }
            }
        }
    });

    let spec = TaskSpec::restartable(worker);

    let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
    sup.run(vec![spec]).await?;

    Ok(())
}
