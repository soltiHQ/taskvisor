//! # Pipeline
//!
//! Uses the optional `controller` feature to demonstrate slot-based admission control.
//! This is useful when you need to limit concurrency per logical operation - e.g.,
//! "only one deploy at a time" or "queue report generation".
//!
//! ## What is the controller?
//!
//! The controller is a thin layer over the supervisor that groups tasks into **slots** (keyed by task name).
//! Each slot enforces an admission policy:
//!
//! | Policy          | Behavior                                             | Use case           |
//! |-----------------|------------------------------------------------------|--------------------|
//! | `Queue`         | FIFO - new task waits until the current one finishes | Job queue          |
//! | `Replace`       | Cancels running task, starts new one                 | Search-as-you-type |
//! | `DropIfRunning` | Silently ignores if slot is busy                     | Debounced actions  |
//!
//! The slot key is the task name (`TaskSpec::name()`).
//! **Tasks with different names go to different slots and never interfere.**
//!
//! ## What this shows
//!
//! - `Supervisor::builder(cfg).with_controller(config).build()` - enables the controller feature. Requires `--features controller`.
//! - Three demos: Queue (sequential), Replace (latest-wins), DropIfRunning (reject-while-busy).
//! - `handle.submit(ControllerSpec::queue(spec))`: submit to a slot.
//!
//! ## How it differs from `handle.add()`
//!
//! `add` registers a task directly in the registry: no slot, no admission policy.
//! `submit` goes through the controller, which manages the slot lifecycle and decides whether to accept, queue, or reject.
//!
//! ## Enabling the feature
//!
//! ```toml
//! [dependencies]
//! taskvisor = { version = "...", features = ["controller"] }
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
//! cargo run --example pipeline --features controller
//! ```

#[cfg(not(feature = "controller"))]
compile_error!(
    "This example requires the `controller` feature: cargo run --example pipeline --features controller"
);

use std::time::Duration;

use taskvisor::prelude::*;

fn job(name: &'static str, duration: Duration) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(name, move |ctx: CancellationToken| async move {
        println!("  [{name}] started");
        let start = tokio::time::Instant::now();

        tokio::select! {
            _ = tokio::time::sleep(duration) => {
                println!("  [{name}] completed in {:?}", start.elapsed());
                Ok(())
            }
            _ = ctx.cancelled() => {
                println!("  [{name}] cancelled after {:?}", start.elapsed());
                Err(TaskError::Canceled)
            }
        }
    });
    TaskSpec::once(task)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sup = Supervisor::builder(SupervisorConfig::default())
        .with_controller(taskvisor::ControllerConfig::default())
        .build();

    // serve() returns a handle for dynamic task submission.
    let handle = sup.serve();

    // Queue: tasks run sequentially
    println!("=== Queue Policy ===");
    println!("Submit 3 jobs with the same name — they run one-by-one.\n");

    for i in 1..=3 {
        let spec = job("queued-job", Duration::from_millis(400));
        handle
            .submit(taskvisor::ControllerSpec::queue(spec))
            .await?;
        println!("  submitted #{i}");
    }
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Replace: new task cancels the running one
    println!("\n=== Replace Policy ===");
    println!("Submit a long job, then replace it with a short one.\n");

    let long = job("replace-job", Duration::from_secs(5));
    handle
        .submit(taskvisor::ControllerSpec::replace(long))
        .await?;

    tokio::time::sleep(Duration::from_millis(300)).await;

    let short = job("replace-job", Duration::from_millis(200));
    handle
        .submit(taskvisor::ControllerSpec::replace(short))
        .await?;

    tokio::time::sleep(Duration::from_secs(1)).await;

    // DropIfRunning: new tasks are ignored while slot is busy
    println!("\n=== DropIfRunning Policy ===");
    println!("Submit a job, then try to submit another while the first is running.\n");

    let first = job("drop-job", Duration::from_millis(600));
    handle
        .submit(taskvisor::ControllerSpec::drop_if_running(first))
        .await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    let second = job("drop-job", Duration::from_millis(100));
    handle
        .submit(taskvisor::ControllerSpec::drop_if_running(second))
        .await?;
    println!("  (second submission should be silently dropped)");

    tokio::time::sleep(Duration::from_secs(1)).await;

    // Shutdown (consumes the handle)
    println!("\nDone.");
    handle.shutdown().await?;

    Ok(())
}
