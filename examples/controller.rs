//! # Example: Controller Demo
//! Visual demonstration of the controller’s slot-based admission model.
//!
//! ```text
//!         ┌───────────────┐
//!         │  application  │
//!         │ (user submits)│
//!         └───────┬───────┘
//!            submit(...)
//!                 ▼
//!         ┌───────────────────┐
//!         │    controller     │
//!         │ (admission logic) │
//!         └───────┬───────────┘
//!            publishes events
//!                 ▼
//!         ┌───────────────────┐
//!         │    supervisor     │
//!         │  (orchestrator)   │
//!         └───────┬───────────┘
//!             spawns actors
//!                 ▼
//!         ┌───────────────────┐
//!         │    task actor     │
//!         │ (run / retry loop)│
//!         └───────────────────┘
//! ```
//!
//! Demonstrates the controller's admission policies:
//! - Queue: tasks execute sequentially (same slot name)
//! - Replace: new submission cancels running task (latest wins)
//! - DropIfRunning: new submission ignored if slot busy
//!
//! Shows how controller events are published and can be observed via `LogWriter`.
//!
//! ## Run
//! ```bash
//! cargo run --example basic_controller --features "controller,logging"
//! ```
#[cfg(not(feature = "controller"))]
compile_error!(
    "This example requires the 'controller' feature. Run with: --features controller,logging"
);

#[cfg(not(feature = "logging"))]
compile_error!(
    "This example requires the 'logging' feature. Run with: --features controller,logging"
);

use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

use taskvisor::LogWriter;
use taskvisor::{
    BackoffPolicy, Config, RestartPolicy, Supervisor, TaskError, TaskFn, TaskRef, TaskSpec,
};
use taskvisor::{ControllerConfig, ControllerSpec};

/// Creates a task that simulates work.
fn make_worker(name: &'static str, work_ms: u64) -> TaskSpec {
    let task_name = name.to_string();

    let task: TaskRef = TaskFn::arc(name, move |ctx: CancellationToken| {
        let name = task_name.clone();

        async move {
            println!("[{name}] started (work={work_ms}ms)");
            let start = tokio::time::Instant::now();

            loop {
                if ctx.is_cancelled() {
                    println!("[{name}] cancelled after {:?}", start.elapsed());
                    return Ok::<(), TaskError>(());
                }
                if start.elapsed().as_millis() >= work_ms as u128 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            println!("[{name}] completed in {:?}", start.elapsed());
            Ok(())
        }
    });
    TaskSpec::new(
        task,
        RestartPolicy::Never,
        BackoffPolicy::default(),
        Some(Duration::from_secs(10)),
    )
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    println!("=== Controller Demo ===\n");

    let sup = Supervisor::builder(Config::default())
        .with_subscribers(vec![Arc::new(LogWriter::default())])
        .with_controller(ControllerConfig {
            queue_capacity: 100,
            slot_capacity: 10,
        })
        .build();

    // Spawn supervisor in background
    let sup_clone = Arc::clone(&sup);
    let sup_task = tokio::spawn(async move {
        if let Err(e) = sup_clone.run(vec![]).await {
            eprintln!("Supervisor error: {e}");
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    // === Demo 1: Queue Policy (sequential execution in the SAME slot) ===
    println!("\n--- Demo 1: Queue Policy (sequential execution) ---");
    sup.submit(ControllerSpec::queue(make_worker("build", 500)))
        .await?;
    sup.submit(ControllerSpec::queue(make_worker("build", 500)))
        .await?;
    sup.submit(ControllerSpec::queue(make_worker("build", 500)))
        .await?;
    tokio::time::sleep(Duration::from_millis(2000)).await;

    // === Demo 2: Replace Policy (cancel current and start latest on terminal) ===
    println!("\n--- Demo 2: Replace Policy (cancel and restart) ---");
    sup.submit(ControllerSpec::replace(make_worker("deploy", 1000)))
        .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    sup.submit(ControllerSpec::replace(make_worker("deploy", 1000)))
        .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    sup.submit(ControllerSpec::replace(make_worker("deploy", 1000)))
        .await?;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // === Demo 3: DropIfRunning Policy (ignore if busy) ===
    println!("\n--- Demo 3: DropIfRunning Policy (ignore if busy) ---");
    sup.submit(ControllerSpec::drop_if_running(make_worker("health", 800)))
        .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // These will be ignored (same slot is running)
    sup.submit(ControllerSpec::drop_if_running(make_worker("health", 800)))
        .await?;
    sup.submit(ControllerSpec::drop_if_running(make_worker("health", 800)))
        .await?;
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // After slot is idle, this will execute
    sup.submit(ControllerSpec::drop_if_running(make_worker("health", 800)))
        .await?;
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // === Demo 4: Queue Full (rejections) ===
    println!("\n--- Demo 4: Queue Full (rejection) ---");
    // Same slot "batch": slot_capacity = 10 → 2 submissions will be rejected.
    for _ in 1..=12 {
        sup.submit(ControllerSpec::queue(make_worker("batch", 200)))
            .await?;
    }
    tokio::time::sleep(Duration::from_secs(4)).await;

    println!("\n--- Demo Complete ---");
    drop(sup);
    let _ = sup_task.await;
    Ok(())
}
