//! # Dynamic task management
//!
//! Use `Supervisor::serve` when tasks are discovered after the runtime starts.
//! It returns a handle for registration, inspection, stopping, and joined shutdown.
//!
//! ```text
//! application ──► serve ──► SupervisorHandle
//!                                 ├── add(...).execute ───► registry ──► task attempts
//!                                 ├── remove(...).execute ► claim a stop and return
//!                                 ├── cancel(...).execute ► wait for cleanup
//!                                 └── shutdown ──► close admission and wait
//! ```
//!
//! `add(...).execute().await` confirms registry admission.
//! It does not confirm that an attempt started or finished.
//! `list` reads registry membership. `is_alive` reads physical attempt activity directly.
//! Neither query depends on lifecycle-event delivery.
//!
//! `remove(...).execute().await` returns after it claims a stop, before registered-task cleanup finishes.
//! `cancel(...).execute().await` waits for bounded logical cleanup. Here, cancellation follows an earlier removal.
//! It joins that stop or observes completed cleanup.
//! It returns `false` in either case because it did not create the original claim.
//!
//! Expect worker ticks, three `Registered:` snapshots, and cancellation messages.
//! The example requests shutdown itself and exits after a few seconds.
//!
//! Run with `cargo run --example dynamic_tasks`.

use std::time::Duration;

use taskvisor::prelude::*;

fn make_worker(name: &'static str) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(move |ctx| async move {
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
    TaskSpec::restartable(name, task)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);

    // serve() starts the runtime without OS signal handlers and returns its management handle.
    let handle = supervisor.serve()?;

    // Add workers dynamically
    println!("Adding worker-a and worker-b...");
    let id_a = handle.add(make_worker("worker-a")).execute().await?;
    let id_b = handle.add(make_worker("worker-b")).execute().await?;

    // Demo pacing only: execute().await has already confirmed registration.
    tokio::time::sleep(Duration::from_secs(1)).await;
    println!("Registered: {:?}", handle.list().await);

    // remove(...).execute().await claims the stop but returns before registered-task cleanup ends.
    println!("\nRemoving worker-a...");
    let removed = handle.remove(id_a).execute().await?;
    println!("worker-a removal claimed: {removed}");

    // Join the same removal before reading authoritative registry state.
    // This returns false because the removal already claimed the stop (or cleanup finished first).
    let second_claim = handle.cancel(id_a).execute().await?;
    println!("worker-a second cancellation claimed: {second_claim}");
    println!("Registered: {:?}", handle.list().await);

    // Add worker-c
    println!("\nAdding worker-c...");
    handle.add(make_worker("worker-c")).execute().await?;
    // Demo pacing only: let the terminal show a worker-c tick.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Cancel worker-b
    println!("Cancelling worker-b...");
    let cancelled = handle.cancel(id_b).execute().await?;
    println!("worker-b cancelled: {cancelled}");
    // This direct query reads physical attempt activity. It does not consume lifecycle events.
    println!(
        "worker-b physically active: {}",
        handle.is_alive("worker-b").await
    );

    // Demo pacing only: let worker-c keep ticking before the final snapshot.
    tokio::time::sleep(Duration::from_millis(500)).await;
    println!("\nRegistered: {:?}", handle.list().await);

    // Graceful shutdown (consumes the handle)
    println!("\nShutting down...");
    handle.shutdown().await?;
    Ok(())
}
