//! # Slot admission policies
//!
//! The controller groups tasks by a slot key.
//! One task can run in a slot at a time.
//! Different slot keys are independent.
//!
//! | Policy          | When the slot is busy                       |
//! |-----------------|---------------------------------------------|
//! | `Queue`         | wait in FIFO order                          |
//! | `Replace`       | cancel the owner and replace the queue head |
//! | `DropIfRunning` | reject the new submission                   |
//!
//! The slot key defaults to the task name.
//! `with_slot` can set an explicit key.
//! `add` bypasses the controller; `submit` uses its admission rules.
//! An `Ok(id)` from `submit` confirms intake, not final admission.
//! Use `submit_and_watch` when application logic needs the final result.
//! `Replace` does not clear the full FIFO queue; items behind the head remain.
//!
//! Run with `cargo run --example slots`.

use std::sync::Arc;
use std::time::Duration;

use taskvisor::prelude::*;
use taskvisor::{ControllerConfig, ControllerSpec};
use tokio::sync::Notify;

fn job(name: &'static str, duration: Duration) -> TaskSpec {
    job_with_start(name, duration, None)
}

fn job_with_start(
    name: &'static str,
    duration: Duration,
    started: Option<Arc<Notify>>,
) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(name, move |ctx| {
        let started = started.clone();
        async move {
            if let Some(started) = &started {
                started.notify_one();
            }
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
        }
    });
    TaskSpec::once(task)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let supervisor = Supervisor::builder(SupervisorConfig::default())
        .with_controller(ControllerConfig::default())
        .build();

    // serve() returns a handle for dynamic task submission.
    let handle = supervisor.serve();

    // Queue: tasks run sequentially
    println!("=== Queue Policy ===");
    println!("Submit 3 jobs with the same name — they run one-by-one.\n");

    let mut queued = Vec::new();
    for i in 1..=3 {
        let spec = job("queued-job", Duration::from_millis(400));
        let (_id, waiter) = handle.submit_and_watch(ControllerSpec::queue(spec)).await?;
        queued.push(waiter);
        println!("  submitted #{i}");
    }
    for (index, waiter) in queued.into_iter().enumerate() {
        println!("  queued #{} -> {:?}", index + 1, waiter.wait().await?);
    }

    // Replace: new task cancels the running one
    println!("\n=== Replace Policy ===");
    println!("Submit a long job, then replace it with a short one.\n");

    let long_started = Arc::new(Notify::new());
    let long = job_with_start(
        "replace-job",
        Duration::from_secs(5),
        Some(Arc::clone(&long_started)),
    );
    let (_long_id, long_waiter) = handle
        .submit_and_watch(ControllerSpec::replace(long))
        .await?;
    long_started.notified().await;

    let short = job("replace-job", Duration::from_millis(200));
    let (_short_id, short_waiter) = handle
        .submit_and_watch(ControllerSpec::replace(short))
        .await?;
    println!("  long -> {:?}", long_waiter.wait().await?);
    println!("  short -> {:?}", short_waiter.wait().await?);

    // DropIfRunning: new tasks are rejected while the slot is busy
    println!("\n=== DropIfRunning Policy ===");
    println!("Submit a job, then try to submit another while the first is running.\n");

    let first_started = Arc::new(Notify::new());
    let first = job_with_start(
        "drop-job",
        Duration::from_millis(600),
        Some(Arc::clone(&first_started)),
    );
    let (_first_id, first_waiter) = handle
        .submit_and_watch(ControllerSpec::drop_if_running(first))
        .await?;
    first_started.notified().await;

    let second = job("drop-job", Duration::from_millis(100));
    let (_second_id, second_waiter) = handle
        .submit_and_watch(ControllerSpec::drop_if_running(second))
        .await?;
    println!("  second -> {:?}", second_waiter.wait().await?);
    println!("  first -> {:?}", first_waiter.wait().await?);

    // Joined shutdown consumes the handle.
    handle.shutdown().await?;

    Ok(())
}
