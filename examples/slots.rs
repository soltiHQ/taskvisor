//! # Slot admission policies
//!
//! The optional controller groups tasks by a slot key. One task can run in a
//! slot at a time. Different slot keys are independent.
//!
//! | Policy | When the slot is busy |
//! |--------|-----------------------|
//! | `Queue` | wait in FIFO order |
//! | `Replace` | cancel the owner and replace the queue head |
//! | `DropIfRunning` | reject the new submission |
//!
//! The slot key defaults to the task name. `with_slot` can set an explicit key.
//! `add` bypasses the controller; `submit` uses its admission rules. An
//! `Ok(id)` from `submit` confirms intake, not final admission. Use
//! `submit_and_watch` when application logic needs the final result.
//! `Replace` does not clear the full FIFO queue; items behind the head remain.
//!
//! Run with `cargo run --example slots --features controller`.

#[cfg(not(feature = "controller"))]
compile_error!(
    "This example requires the `controller` feature: cargo run --example slots --features controller"
);

use std::time::Duration;

use taskvisor::prelude::*;

fn job(name: &'static str, duration: Duration) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(name, move |ctx| async move {
        println!("  [{name}] started");
        let start = tokio::time::Instant::now();

        match ctx.run_until_cancelled(tokio::time::sleep(duration)).await {
            Ok(()) => {
                println!("  [{name}] completed in {:?}", start.elapsed());
                Ok(())
            }
            Err(canceled) => {
                println!("  [{name}] cancelled after {:?}", start.elapsed());
                Err(canceled)
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
    println!("  (the controller should reject the second submission)");

    tokio::time::sleep(Duration::from_secs(1)).await;

    // Shutdown (consumes the handle)
    println!("\nDone.");
    handle.shutdown().await?;

    Ok(())
}
