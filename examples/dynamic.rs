//! # Dynamic
//!
//! Adds, removes, and cancels tasks while the supervisor is running.
//!
//! This is the pattern for applications where the set of tasks is not known at startup.
//!
//! *e.g., HTTP servers spawning a worker per request, job queues, or interactive CLIs.*
//!
//! ## Two modes of operation
//!
//! Taskvisor has two distinct entry points:
//!
//! | Method                             | When to use            | Lifecycle                   |
//! |------------------------------------|------------------------|-----------------------------|
//! | `sup.run(specs)`                   | Tasks known upfront    | Blocks until done or Ctrl+C |
//! | `sup.serve()` → `SupervisorHandle` | Tasks added at runtime | You control shutdown        |
//!
//! ### `run()`: "Fire and forget"
//!
//! You know all your tasks at startup.
//!
//! The supervisor owns the lifecycle: it blocks, handles Ctrl+C, and shuts down automatically.
//!
//! Typical use cases:
//! - Microservice with a fixed set of background workers (metrics exporter, health checker, queue consumer)
//! - CLI tool that processes a batch of files in parallel
//! - Periodic cron-like jobs defined in config at startup
//!
//! ### `serve()`: "I'll manage it"
//!
//! Tasks appear and disappear at runtime.
//!
//! You get a `SupervisorHandle` and call `shutdown()` when you're done.
//!
//! Typical use cases:
//! - HTTP server that spawns a background job per request
//! - Plugin system where plugins register tasks dynamically
//! - Chat system where each connected user gets a dedicated task
//! - Job queue consumer that creates a task per incoming message
//! - Interactive CLI / REPL where user commands start/stop tasks
//!
//! ## What this shows
//!
//! - `sup.serve()`: starts listeners, returns a handle. Non-blocking.
//! - `handle.add(spec)`: register a new task dynamically.
//! - `handle.remove(name)`: cancel and deregister by name.
//! - `handle.cancel(name)`: cancel and wait for confirmation.
//! - `handle.list()`: snapshot of active task names.
//! - `handle.is_alive(name)`: check if a specific task is running.
//! - `handle.shutdown()`: graceful stop (consumes the handle).
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
//! cargo run --example dynamic
//! ```
//!
//! ## Next
//!
//! | Example                      | What it adds                                    |
//! |------------------------------|-------------------------------------------------|
//! | [`pipeline.rs`](pipeline.rs) | Admission control with the `controller` feature |

use std::time::Duration;

use taskvisor::prelude::*;

fn make_worker(name: &'static str) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(name, move |ctx: CancellationToken| async move {
        let mut tick = 0u32;
        loop {
            tokio::select! {
                _ = ctx.cancelled() => {
                    println!("  [{name}] stopped at tick #{tick}");
                    return Ok(());
                }
                _ = tokio::time::sleep(Duration::from_millis(300)) => {
                    tick += 1;
                    println!("  [{name}] tick #{tick}");
                }
            }
        }
    });
    TaskSpec::restartable(task)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sup = Supervisor::new(SupervisorConfig::default(), vec![]);

    // serve() starts listeners and returns a handle for dynamic management.
    let handle = sup.serve();

    // Add workers dynamically
    println!("Adding worker-a and worker-b...");
    handle.add(make_worker("worker-a"))?;
    handle.add(make_worker("worker-b"))?;

    tokio::time::sleep(Duration::from_secs(1)).await;
    println!("Active: {:?}", handle.list().await);

    // Remove worker-a
    println!("\nRemoving worker-a...");
    handle.remove("worker-a")?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    println!("Active: {:?}", handle.list().await);

    // Add worker-c
    println!("\nAdding worker-c...");
    handle.add(make_worker("worker-c"))?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Cancel worker-b
    println!("Cancelling worker-b...");
    let cancelled = handle.cancel("worker-b").await?;
    println!("worker-b cancelled: {cancelled}");
    println!("worker-b alive: {}", handle.is_alive("worker-b").await);

    tokio::time::sleep(Duration::from_millis(500)).await;
    println!("\nActive: {:?}", handle.list().await);

    // Graceful shutdown (consumes the handle)
    println!("\nShutting down...");
    handle.shutdown().await?;
    println!("Done.");
    Ok(())
}
