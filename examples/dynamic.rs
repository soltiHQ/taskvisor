//! # Dynamic task management
//!
//! Use `Supervisor::serve` when long-lived tasks are discovered at runtime.
//! Examples include tenant workers, plugins, and connections that own a
//! resident background loop. Taskvisor is not a per-request job executor.
//!
//! This example shows how to:
//!
//! - add a task and receive its `TaskId`;
//! - inspect the current registry snapshot;
//! - cancel or remove a task by identity;
//! - shut down the supervisor explicitly.
//!
//! `Supervisor::run` is the simpler choice when all tasks are known at startup.
//! It waits for the supervisor to finish. `serve` returns a handle immediately,
//! so the caller owns the shutdown flow.
//!
//! Run with `cargo run --example dynamic`.

use std::time::Duration;

use taskvisor::prelude::*;

fn make_worker(name: &'static str) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(name, move |ctx| async move {
        let mut tick = 0u32;
        loop {
            match ctx
                .run_until_cancelled(tokio::time::sleep(Duration::from_millis(300)))
                .await
            {
                Ok(()) => {
                    tick += 1;
                    println!("  [{name}] tick #{tick}");
                }
                Err(canceled) => {
                    println!("  [{name}] stopped at tick #{tick}");
                    return Err(canceled);
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
    let id_a = handle.add(make_worker("worker-a")).await?;
    let id_b = handle.add(make_worker("worker-b")).await?;

    tokio::time::sleep(Duration::from_secs(1)).await;
    println!("Active: {:?}", handle.list().await);

    // remove() claims the stop but returns before registered-task cleanup ends.
    println!("\nRemoving worker-a...");
    let removed = handle.remove(id_a).await?;
    println!("worker-a removal claimed: {removed}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    println!("Active: {:?}", handle.list().await);

    // Add worker-c
    println!("\nAdding worker-c...");
    handle.add(make_worker("worker-c")).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Cancel worker-b
    println!("Cancelling worker-b...");
    let cancelled = handle.cancel(id_b).await?;
    println!("worker-b cancelled: {cancelled}");
    println!(
        "worker-b alive (best-effort event view): {}",
        handle.is_alive("worker-b").await
    );

    tokio::time::sleep(Duration::from_millis(500)).await;
    println!("\nActive: {:?}", handle.list().await);

    // Graceful shutdown (consumes the handle)
    println!("\nShutting down...");
    handle.shutdown().await?;
    println!("Done.");
    Ok(())
}
