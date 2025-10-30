//! # Runtime Control Example
//!
//! Shows how to add, remove, and cancel tasks while supervisor is running.
//!
//! Demonstrates:
//! - Adding tasks dynamically
//! - Removing tasks by name
//! - Checking task status
//!
//! ## Run
//! ```bash
//! cargo run --example control
//! ```

use std::{sync::Arc, time::Duration};
use taskvisor::{
    BackoffPolicy, Config, RestartPolicy, Supervisor, TaskError, TaskFn, TaskRef, TaskSpec,
};
use tokio_util::sync::CancellationToken;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let sup = Arc::new(Supervisor::new(Config::default(), vec![]));
    let runner = Arc::clone(&sup);
    tokio::spawn(async move {
        let _ = runner.run(vec![]).await;
    });
    sup.wait_ready().await;

    // ============================================================
    // Demo 1: Add task dynamically
    // ============================================================
    println!(" ─► Adding 'worker-A'...");

    sup.add_task(make_worker("worker-A"))?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let tasks = sup.list_tasks().await;
    println!(" ─► Active tasks: {tasks:?}");

    // ============================================================
    // Demo 2: Add second task
    // ============================================================
    println!(" ─► Adding 'worker-B'...");

    sup.add_task(make_worker("worker-B"))?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let tasks = sup.list_tasks().await;
    println!(" ─► Active tasks: {tasks:?}");

    // ============================================================
    // Demo 3: Remove specific task
    // ============================================================
    println!(" ─► Removing 'worker-A'...");

    sup.remove_task("worker-A")?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let tasks = sup.list_tasks().await;
    println!(" ─► Active tasks: {tasks:?}");

    // ============================================================
    // Demo 4: Cancel task (with confirmation)
    // ============================================================
    println!(" ─► Cancelling 'worker-B'...");

    let cancelled = sup.cancel("worker-B").await?;
    println!(" ─► Task cancelled: {cancelled}");

    let alive = sup.is_alive("worker-B").await;
    println!(" ─► Is alive: {alive}");

    println!("Done");
    Ok(())
}

fn make_worker(name: &'static str) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(name, move |ctx: CancellationToken| async move {
        println!("{:>4}[{name}] started", "");

        let mut counter = 0u32;
        loop {
            if ctx.is_cancelled() {
                println!("{:>4}[{name}] cancelled", "");
                return Err(TaskError::Canceled);
            }

            counter += 1;
            println!("{:>4}[{name}] tick #{counter}", "");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
    TaskSpec::new(task, RestartPolicy::Always, BackoffPolicy::default(), None)
}
