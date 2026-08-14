//! # Outcomes: wait for the final result
//!
//! Use `add_and_watch` when application logic must know how one task ended.
//! Its `TaskWaiter` uses a dedicated terminal channel, separate from best-effort events.
//!
//! ```text
//! add_and_watch ──────► TaskId + TaskWaiter
//! managed lifecycle ──► terminal outcome ────► TaskWaiter
//! lifecycle events ───► event bus ───────────► subscribers (best-effort)
//! ```
//!
//! | Task behavior             | Final outcome |
//! |---------------------------|---------------|
//! | succeeds                  | `Completed`   |
//! | exhausts its retry budget | `Failed`      |
//! | times out under `once`    | `Failed`      |
//! | returns a permanent error | `Fatal`       |
//! | observes cancellation     | `Canceled`    |
//!
//! A configured timeout is retry-eligible.
//! The timeout row is final because `once` forbids retry.
//! `with_max_retries(NonZeroU32::new(2).unwrap())` allows the first failed attempt and two retries before `Failed`.
//! An admitted result normally arrives after registry membership is removed.
//! Except for `ForceAborted`, task execution is physically joined first.
//! `ForceAborted` is final logically while a physical actor may still be active.
//! Controller rejection can resolve without starting the task.
//! `Panicked` covers an actor or protected cleanup panic.
//!
//! Expect one printed section for each table row. The example then shuts down and exits.
//! `TaskWaiter::wait` can return an error if its terminal channel closes unexpectedly.
//!
//! Run with `cargo run --example outcomes`.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use taskvisor::prelude::*;
use tokio::sync::Notify;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    let handle = supervisor.serve()?;

    // 1) A one-shot job that succeeds -> Completed.
    println!("=== Completed ===");
    let job: TaskRef = TaskFn::arc(|_ctx| async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        Ok(())
    });
    let (_id, waiter) = handle.add_and_watch(TaskSpec::once("import", job)).await?;
    println!("  import -> {:?}\n", waiter.wait().await?);

    // 2) A task that always fails, with a bounded retry budget -> Failed.
    //    Its reason/exit_code are identical to the typed TaskFinished event.
    println!("=== Failed (retries exhausted) ===");
    let attempts = Arc::new(AtomicU32::new(0));
    let flaky: TaskRef = TaskFn::arc(move |_ctx| {
        let attempts = Arc::clone(&attempts);
        async move {
            let n = attempts.fetch_add(1, Ordering::Relaxed) + 1;
            println!("  sync attempt #{n} failing...");
            Err(TaskError::fail("upstream 503").with_exit_code(75))
        }
    });
    let spec = TaskSpec::restartable("sync", flaky)
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

    // 3) A one-shot task that exceeds its per-attempt deadline -> Failed.
    println!("=== Failed (attempt timed out) ===");
    let slow: TaskRef = TaskFn::arc(|_ctx| async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        Ok(())
    });
    let timed = TaskSpec::once("slow-report", slow).with_timeout(Duration::from_millis(20));
    match handle.add_and_watch(timed).await?.1.wait().await? {
        TaskOutcome::Failed { reason, .. } => {
            println!("  slow-report -> Failed: {reason}\n");
        }
        other => println!("  slow-report -> {other:?}\n"),
    }

    // 4) A permanent error stops even a restartable task -> Fatal.
    println!("=== Fatal (not retryable) ===");
    let permanent: TaskRef = TaskFn::arc(|_ctx| async {
        Err::<(), TaskError>(TaskError::fatal("credentials rejected").with_exit_code(78))
    });
    let fatal = TaskSpec::restartable("credential-check", permanent);
    match handle.add_and_watch(fatal).await?.1.wait().await? {
        TaskOutcome::Fatal {
            reason, exit_code, ..
        } => {
            println!("  credential-check -> Fatal: {reason} (exit_code={exit_code:?})\n");
        }
        other => println!("  credential-check -> {other:?}\n"),
    }

    // 5) A long-running worker we cancel -> Canceled.
    println!("=== Canceled ===");
    let started = Arc::new(Notify::new());
    let worker: TaskRef = TaskFn::arc({
        let started = Arc::clone(&started);
        move |ctx| {
            let started = Arc::clone(&started);
            async move {
                started.notify_one();
                ctx.cancelled().await;
                Err(TaskError::Canceled)
            }
        }
    });
    let (id, waiter) = handle
        .add_and_watch(TaskSpec::restartable("worker", worker))
        .await?;
    // The task body, rather than a timer, confirms that the worker started.
    started.notified().await;
    println!("  cancelling worker...");
    handle.cancel(id).await?;
    println!("  worker -> {:?}\n", waiter.wait().await?);

    handle.shutdown().await?;
    Ok(())
}
