//! # Admission
//!
//! Awaits the outcome of a **controller** submission with `submit_and_watch`.
//!
//! This is the slot-based counterpart of [`outcomes.rs`](outcomes.rs)'s `add_and_watch`:
//! the controller decides whether to admit, queue, or reject each submission, and the returned `TaskWaiter` resolves to the final [`TaskOutcome`] either way.
//!
//! ## The key distinction: admitted vs. rejected
//!
//! `handle.submit(spec)` is fire-and-forget: `Ok(id)` means *enqueued*, not *admitted*.
//! The admission decision is asynchronous. `submit_and_watch` lets you await the result:
//!
//! - If the slot **admits** the task, the waiter resolves to its real terminal outcome (`Completed`, `Failed`, `Canceled`, ...) once it has run.
//! - If the slot **never admits** it - busy under `DropIfRunning`, queue full, superseded by a later `Replace`, removed while queued, or shutting down
//!   The waiter resolves to [`TaskOutcome::Rejected`], carrying the reason. The task body never ran.
//!
//! This closes a real gap: with plain `submit`, a rejected submission is only visible as a `ControllerRejected` event on the lossy bus.
//! With `submit_and_watch` the rejection is a guaranteed, awaitable result.
//!
//! ## What this shows
//!
//! - `handle.submit_and_watch(ControllerSpec::...)`: submit and get `(TaskId, TaskWaiter)`.
//! - An **admitted** `Queue` job resolving to `Completed`.
//! - A **rejected** `DropIfRunning` job (slot busy) resolving to `Rejected { reason }`.
//! - `handle.controller_snapshot()`: pull the live slot state (status, queue depth) without parsing events.
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
//! cargo run --example admission --features controller
//! ```
//!
//! ## Next
//!
//! | Example                      | What it adds                                   |
//! |------------------------------|------------------------------------------------|
//! | [`pipeline.rs`](pipeline.rs) | All three admission policies, side by side     |
//! | [`outcomes.rs`](outcomes.rs) | The direct-path waiter (`add_and_watch`)       |

#[cfg(not(feature = "controller"))]
compile_error!(
    "This example requires the `controller` feature: cargo run --example admission --features controller"
);

use std::time::Duration;

use taskvisor::ControllerSpec;
use taskvisor::prelude::*;

/// A job that runs for `dur`, observing cancellation.
fn job(name: &'static str, dur: Duration) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(name, move |ctx: TaskContext| async move {
        tokio::select! {
            _ = tokio::time::sleep(dur) => Ok(()),
            _ = ctx.cancelled() => Err(TaskError::Canceled),
        }
    });
    // Same slot "deploy" for every submission; they contend for one slot.
    TaskSpec::once(task)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sup = Supervisor::builder(SupervisorConfig::default())
        .with_controller(taskvisor::ControllerConfig::default())
        .build();
    let handle = sup.serve();

    println!("Slot 'deploy' admits at most one task at a time.\n");

    // 1) The slot is idle: this submission is admitted and starts running.
    println!("1) submit deploy-v1 (Queue) to the idle slot");
    let (_id, v1) = handle
        .submit_and_watch(
            ControllerSpec::queue(job("deploy-v1", Duration::from_millis(200))).with_slot("deploy"),
        )
        .await?;
    // Give it a moment to actually occupy the slot.
    tokio::time::sleep(Duration::from_millis(50)).await;
    println!("    deploy-v1 admitted, now running\n");

    // Pull the controller's live state directly: no parsing of bus events.
    if let Some(snap) = handle.controller_snapshot().await {
        let deploy = snap.slot("deploy");
        println!(
            "    controller: {} running, {} queued; deploy status={:?} depth={}\n",
            snap.running_count(),
            snap.total_queued(),
            deploy.map(|s| s.status),
            deploy.map_or(0, |s| s.queue_depth),
        );
    }

    // 2) While deploy-v1 holds the slot, a DropIfRunning submission is refused.
    //     submit() would only fire a ControllerRejected event on the lossy bus;
    //     submit_and_watch() lets us *await* the rejection as a guaranteed result.
    println!("2) submit deploy-v2 (DropIfRunning) while the slot is busy");
    let (_id, v2) = handle
        .submit_and_watch(
            ControllerSpec::drop_if_running(job("deploy-v2", Duration::from_millis(200)))
                .with_slot("deploy"),
        )
        .await?;
    match v2.wait().await? {
        TaskOutcome::Rejected { reason } => {
            println!("    deploy-v2 -> Rejected ({reason}) — never ran\n");
        }
        other => println!("    deploy-v2 -> {other:?} (unexpected)\n"),
    }

    // 3) The admitted task still finishes normally.
    println!("3) await the admitted task");
    println!("    deploy-v1 -> {:?}", v1.wait().await?);

    handle.shutdown().await?;
    println!("\nDone.");
    Ok(())
}
