//! # CPU Job: Heavy Work Under Supervision
//!
//! Async runtimes must not run heavy CPU work on their worker threads: it blocks the executor.
//! The common fix is the "rayon bridge": offload the computation to a CPU pool and receive the result back through a oneshot channel.
//!
//! This example puts that bridge **inside** a supervised task.
//! Rayon does the computing, Tokio stays free, and taskvisor adds what the bridge alone does not have: restart on failure with backoff, and an awaitable final outcome.
//!
//! ## The pattern
//!
//! ```text
//! task body:  rayon::spawn(compute) ──► await oneshot ──► Ok / Err
//! on Err:     supervisor restarts the task with backoff (a fresh rayon job)
//! on finish:  the caller awaits the final outcome via add_and_watch
//! ```
//!
//! ## What this shows
//!
//! - Offloading CPU-bound work from a supervised task without blocking Tokio.
//! - Awaiting the job's final result after the supervisor retries it.
//! - Mapping a computation error into a retryable task failure.
//!
//! ## One honest limit
//!
//! Cancellation drops the receiver, not the computation.
//! A rayon job cannot be interrupted mid-flight: on shutdown it finishes in the background and its result is dropped.
//!
//! ## Run
//!
//! ```bash
//! cargo run --example cpu_job
//! ```
//!
//! ## Next
//!
//! | Example                                    | What it adds                            |
//! |--------------------------------------------|-----------------------------------------|
//! | [`outcomes.rs`](outcomes.rs)               | All terminal outcomes, side by side     |
//! | [`queue_consumer.rs`](queue_consumer.rs)   | The same supervision for an I/O worker  |

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use taskvisor::prelude::*;
use tokio::sync::oneshot;

/// Simulated heavy computation: sum of all primes below `limit` (naive on purpose).
fn sum_of_primes(limit: u64) -> u64 {
    (2..limit)
        .filter(|n| (2..).take_while(|d| d * d <= *n).all(|d| n % d != 0))
        .sum()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
    let handle = sup.serve();

    let attempts = Arc::new(AtomicU32::new(0));
    let job: TaskRef = TaskFn::arc("prime-sum", {
        let attempts = Arc::clone(&attempts);
        move |ctx| {
            let attempts = Arc::clone(&attempts);
            async move {
                // The first attempt fails to show restart + backoff.
                let attempt = attempts.fetch_add(1, Ordering::Relaxed) + 1;

                // The rayon bridge: compute off the runtime, await a oneshot.
                let (tx, rx) = oneshot::channel();
                rayon::spawn(move || {
                    let result = if attempt == 1 {
                        Err("transient compute failure (simulated)".to_string())
                    } else {
                        Ok(sum_of_primes(50_000))
                    };
                    let _ = tx.send(result);
                });

                println!("[prime-sum] attempt #{attempt}: computing on rayon...");

                // `?` exits with TaskError::Canceled on shutdown (clean stop).
                match ctx.run_until_cancelled(rx).await? {
                    Ok(Ok(sum)) => {
                        println!("[prime-sum] done: {sum}");
                        Ok(())
                    }
                    Ok(Err(reason)) => {
                        println!("[prime-sum] failed: {reason}");
                        Err(TaskError::fail(reason))
                    }
                    Err(_dropped) => Err(TaskError::fail("compute thread dropped the channel")),
                }
            }
        }
    });

    let spec = TaskSpec::restartable(job)
        .with_backoff(BackoffPolicy::constant(Duration::from_millis(200)));

    // Await the job's final result: the supervisor retried it for us.
    let (_id, waiter) = handle.add_and_watch(spec, Duration::from_secs(1)).await?;
    println!("outcome: {:?}", waiter.wait().await?);

    handle.shutdown().await?;
    Ok(())
}
