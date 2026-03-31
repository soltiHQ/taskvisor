//! # Basic
//!
//! The simplest possible taskvisor program: **one task, one run, one exit.**
//! Start here to understand the minimal wiring.
//!
//! ## What this shows
//!
//! - `TaskFn::arc` wraps a closure into an `Arc<dyn Task>` (a `TaskRef`).
//! - `TaskSpec::once(task)` creates a one-shot spec (`RestartPolicy::Never`).
//! - `Supervisor::run(specs)` blocks until all tasks finish or Ctrl+C is pressed.
//!
//! The `CancellationToken` parameter is unused here because the task completes instantly.
//!
//! For long-running tasks that **must react to shut down**, see `worker.rs`.
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
//! cargo run --example basic
//! ```
//!
//! ## Next
//!
//! | Example                      | What it adds                             |
//! |------------------------------|------------------------------------------|
//! | [`worker.rs`](worker.rs)     | Long-running task with graceful shutdown |
//! | [`periodic.rs`](periodic.rs) | Cron-like repeated execution             |

use taskvisor::prelude::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let task: TaskRef = TaskFn::arc("hello", |_ctx: CancellationToken| async move {
        println!("Hello from taskvisor!");
        Ok(())
    });

    let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
    sup.run(vec![TaskSpec::once(task)]).await?;

    println!("Done.");
    Ok(())
}
