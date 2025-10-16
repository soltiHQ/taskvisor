//! # Example: cancel_task
//!
//! Demonstrates how to cancel a running task at runtime.
//!
//! Shows how to:
//! - Start a long-running task with [`RestartPolicy::Always`]
//! - Cancel it programmatically using [`Supervisor::cancel`]
//! - Verify task removal via events or registry polling
//!
//! ## Flow
//! ```text
//! main()
//!   ├─► spawn Supervisor::run(vec![long_running_task])
//!   │     ├─► Registry spawns TaskActor
//!   │     └─► Task starts, publishes TaskStarting
//!   │
//!   └─► controller task
//!         ├─► sleep 2 seconds (let task run)
//!         ├─► Supervisor.cancel("worker")
//!         │     ├─► publish TaskRemoveRequested
//!         │     ├─► Registry cancels task token
//!         │     ├─► Task detects cancellation
//!         │     ├─► publish TaskStopped
//!         │     └─► publish TaskRemoved
//!         └─► verify task is gone
//! ```
//!
//! ## Run
//! ```bash
//! cargo run --example cancel_task
//! ```

use std::{sync::Arc, time::Duration};
use taskvisor::{
    BackoffPolicy, Config, RestartPolicy, Subscribe, Supervisor, TaskError, TaskFn, TaskRef,
    TaskSpec,
};
use tokio_util::sync::CancellationToken;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    println!("=== cancel_task example ===\n");

    // 1. Configure runtime
    let mut cfg = Config::default();
    cfg.grace = Duration::from_secs(5);
    cfg.bus_capacity = 256;

    // 2. Optional: add subscriber to see events (requires "logging" feature)
    #[cfg(feature = "logging")]
    let subs: Vec<Arc<dyn Subscribe>> = {
        use taskvisor::LogWriter;
        vec![Arc::new(LogWriter)]
    };
    #[cfg(not(feature = "logging"))]
    let subs: Vec<Arc<dyn Subscribe>> = Vec::new();

    // 3. Create supervisor
    let sup = Arc::new(Supervisor::new(cfg, subs));

    // 4. Define a long-running task that prints every 500ms
    let worker: TaskRef = TaskFn::arc("worker", |ctx: CancellationToken| async move {
        println!("[worker] started, will run until cancelled");

        let mut counter = 0u32;
        loop {
            if ctx.is_cancelled() {
                println!("[worker] detected cancellation, exiting gracefully");
                return Ok::<(), TaskError>(());
            }

            counter += 1;
            println!("[worker] tick #{counter}");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });

    // 5. Create spec with RestartPolicy::Always (so it keeps running until cancelled)
    let spec = TaskSpec::new(
        worker,
        RestartPolicy::Always,
        BackoffPolicy::default(),
        None, // no timeout
    );

    // 6. Run supervisor in background
    let sup_run = {
        let sup = Arc::clone(&sup);
        tokio::spawn(async move {
            println!("[supervisor] starting...\n");
            if let Err(e) = sup.run(vec![spec]).await {
                eprintln!("[supervisor] error: {e}");
            }
            println!("\n[supervisor] stopped");
        })
    };

    // 7. Controller: let task run for 2 seconds, then cancel it
    let controller = {
        let sup = Arc::clone(&sup);
        tokio::spawn(async move {
            // Wait for task to start
            tokio::time::sleep(Duration::from_millis(500)).await;

            // Verify task is running
            println!("\n[controller] checking if 'worker' is alive...");
            let alive = sup.is_alive("worker").await;
            println!("[controller] worker alive: {alive}");
            assert!(alive, "worker should be alive");

            // List active tasks
            let tasks = sup.list_tasks().await;
            println!("[controller] active tasks: {tasks:?}");
            assert!(tasks.contains(&"worker".to_string()));

            // Let it run for 2 seconds
            println!("\n[controller] letting worker run for 2 seconds...");
            tokio::time::sleep(Duration::from_secs(2)).await;

            // Cancel the task
            println!("\n[controller] cancelling 'worker'...");
            match sup.cancel("worker").await {
                Ok(true) => {
                    println!("[controller] worker cancelled successfully");
                }
                Ok(false) => {
                    println!("[controller]  worker was not found (already stopped?)");
                }
                Err(e) => {
                    eprintln!("[controller] error cancelling worker: {e}");
                    return Err(e.into());
                }
            }

            // Verify task is gone
            tokio::time::sleep(Duration::from_millis(200)).await;
            let alive_after = sup.is_alive("worker").await;
            println!("[controller] worker alive after cancel: {alive_after}");
            assert!(!alive_after, "worker should not be alive after cancel");

            let tasks_after = sup.list_tasks().await;
            println!("[controller] active tasks after cancel: {tasks_after:?}");
            assert!(
                !tasks_after.contains(&"worker".to_string()),
                "worker should not be in registry"
            );

            println!("\n[controller] cancellation test passed!");
            Ok::<(), anyhow::Error>(())
        })
    };

    // 8. Wait for controller to finish
    controller.await??;

    // 9. Supervisor should exit naturally (registry is empty)
    let _ = sup_run.await;

    println!("\n=== example completed successfully ===");
    Ok(())
}
