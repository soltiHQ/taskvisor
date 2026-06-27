//! # Outcomes
//!
//! Awaits the **final result** of a supervised task with `add_and_watch` + `TaskWaiter`.
//!
//! This is the supervised analogue of awaiting a `tokio::JoinHandle`:
//! you get back a single, guaranteed `TaskOutcome` once the task has *fully* terminated - after every retry the restart policy allows.
//!
//! ## Two planes, one truth
//!
//! Taskvisor reports task progress on **two** channels:
//!
//! | Plane                          | Delivery                          | Granularity                       |
//! |--------------------------------|-----------------------------------|-----------------------------------|
//! | Events (`Subscribe`)           | Lossy broadcast bus (may drop)    | Every attempt: start/fail/backoff |
//! | Outcome (`add_and_watch`)      | Guaranteed `oneshot` (never lost) | One terminal result per task      |
//!
//! Use **events** for live progress, metrics, and logging.
//! Use the **outcome** when you just need to know how a task ended.
//!
//! ## What this shows
//!
//! - `handle.add_and_watch(spec, timeout)`: register a task and get `(TaskId, TaskWaiter)`.
//! - `waiter.wait().await`: block until the task terminates, returning a `TaskOutcome`.
//! - The three outcomes you will most often see:
//!   - [`TaskOutcome::Completed`] - a one-shot job that succeeded.
//!   - [`TaskOutcome::Failed`] - retries exhausted; carries the final reason + exit code.
//!   - [`TaskOutcome::Canceled`] - stopped via `cancel` / `remove` / shutdown.
//!
//! Other variants exist for the edges:
//!  - `Fatal` - a `TaskError::Fatal`,
//!  - `ForceAborted` - ignored cancellation, killed after the grace period
//!  - `Panicked` - an actor-level panic; panics in the task *body* are caught and surface as `Failed`.
//!
//! ## Controller analogue
//!
//! With the `controller` feature, `handle.submit_and_watch(spec)` is the slot-based counterpart.
//! If a submission is never admitted (slot busy, queue full, superseded, removed, or shutting down) the waiter resolves to `TaskOutcome::Rejected`.
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
//! cargo run --example outcomes
//! ```
//!
//! ## Next
//!
//! | Example                      | What it adds                                    |
//! |------------------------------|-------------------------------------------------|
//! | [`dynamic.rs`](dynamic.rs)   | Add / remove / cancel tasks at runtime          |
//! | [`pipeline.rs`](pipeline.rs) | Slot-based admission control (`controller`)     |

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use taskvisor::prelude::*;

const ADD_TIMEOUT: Duration = Duration::from_secs(1);

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
    let handle = sup.serve();

    // 1) A one-shot job that succeeds -> Completed.
    println!("=== Completed ===");
    let job: TaskRef = TaskFn::arc("import", |_ctx: TaskContext| async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        Ok(())
    });
    let (_id, waiter) = handle
        .add_and_watch(TaskSpec::once(job), ADD_TIMEOUT)
        .await?;
    println!("  import -> {:?}\n", waiter.wait().await?);

    // 2) A task that always fails, with a bounded retry budget -> Failed.
    //    Note the outcome's reason/exit_code are identical to the ActorExhausted event.
    println!("=== Failed (retries exhausted) ===");
    let attempts = Arc::new(AtomicU32::new(0));
    let flaky: TaskRef = TaskFn::arc("sync", move |_ctx: TaskContext| {
        let attempts = Arc::clone(&attempts);
        async move {
            let n = attempts.fetch_add(1, Ordering::Relaxed) + 1;
            println!("  sync attempt #{n} failing...");
            Err(TaskError::fail("upstream 503").with_exit_code(75))
        }
    });
    let spec = TaskSpec::restartable(flaky)
        .with_backoff(BackoffPolicy {
            first: Duration::from_millis(20),
            ..BackoffPolicy::default()
        })
        .with_max_retries(2);
    match handle
        .add_and_watch(spec, ADD_TIMEOUT)
        .await?
        .1
        .wait()
        .await?
    {
        TaskOutcome::Failed {
            reason, exit_code, ..
        } => {
            println!("  sync -> Failed: {reason} (exit_code={exit_code:?})\n");
        }
        other => println!("  sync -> {other:?}\n"),
    }

    // 3) A long-running worker we cancel -> Canceled.
    println!("=== Canceled ===");
    let worker: TaskRef = TaskFn::arc("worker", |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok(())
    });
    let (id, waiter) = handle
        .add_and_watch(TaskSpec::restartable(worker), ADD_TIMEOUT)
        .await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    println!("  cancelling worker...");
    handle.cancel(id).await?;
    println!("  worker -> {:?}\n", waiter.wait().await?);

    handle.shutdown().await?;
    println!("Done.");
    Ok(())
}
