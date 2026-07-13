//! # Outcomes: wait for the final result
//!
//! `add_and_watch` returns a `TaskWaiter`. The waiter resolves after the task
//! entry has stopped, including all restarts allowed by its policy.
//!
//! Taskvisor has two result paths:
//!
//! | Path | Use | Delivery |
//! |------|-----|----------|
//! | lifecycle events | logs, metrics, live progress | bounded and best-effort |
//! | `TaskOutcome` | final business decision | dedicated terminal channel |
//!
//! This example handles successful, failed, and canceled tasks. Other outcomes
//! cover fatal errors, force-abort, actor panic, and controller rejection.
//! `TaskWaiter::wait` can still return an error if the runtime closes the
//! terminal channel unexpectedly.
//!
//! Run with `cargo run --example outcomes`.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use taskvisor::prelude::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
    let handle = sup.serve();

    // 1) A one-shot job that succeeds -> Completed.
    println!("=== Completed ===");
    let job: TaskRef = TaskFn::arc("import", |_ctx| async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        Ok(())
    });
    let (_id, waiter) = handle.add_and_watch(TaskSpec::once(job)).await?;
    println!("  import -> {:?}\n", waiter.wait().await?);

    // 2) A task that always fails, with a bounded retry budget -> Failed.
    //    Note the outcome's reason/exit_code are identical to the ActorExhausted event.
    println!("=== Failed (retries exhausted) ===");
    let attempts = Arc::new(AtomicU32::new(0));
    let flaky: TaskRef = TaskFn::arc("sync", move |_ctx| {
        let attempts = Arc::clone(&attempts);
        async move {
            let n = attempts.fetch_add(1, Ordering::Relaxed) + 1;
            println!("  sync attempt #{n} failing...");
            Err(TaskError::fail("upstream 503").with_exit_code(75))
        }
    });
    let spec = TaskSpec::restartable(flaky)
        .with_backoff(BackoffPolicy::constant(Duration::from_millis(20)))
        .with_max_retries(NonZeroU32::new(2).unwrap());
    match handle.add_and_watch(spec).await?.1.wait().await? {
        TaskOutcome::Failed {
            reason, exit_code, ..
        } => {
            println!("  sync -> Failed: {reason} (exit_code={exit_code:?})\n");
        }
        other => println!("  sync -> {other:?}\n"),
    }

    // 3) A long-running worker we cancel -> Canceled.
    println!("=== Canceled ===");
    let worker: TaskRef = TaskFn::arc("worker", |ctx| async move {
        ctx.cancelled().await;
        Ok(())
    });
    let (id, waiter) = handle.add_and_watch(TaskSpec::restartable(worker)).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    println!("  cancelling worker...");
    handle.cancel(id).await?;
    println!("  worker -> {:?}\n", waiter.wait().await?);

    handle.shutdown().await?;
    println!("Done.");
    Ok(())
}
