//! # CPU-bound work under supervision
//!
//! Heavy CPU work should not run on Tokio worker threads.
//! This example sends the computation to Rayon and waits for its result through a one-shot channel.
//! Taskvisor adds retries, backoff, and an awaitable final outcome around that bridge.
//!
//! There is an important limit: canceling the async task drops the result receiver, but it does not stop a Rayon job already in progress.
//! That job finishes in the CPU pool and its result is discarded.
//!
//! Run with `cargo run --example cpu_job`.

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
    let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    let handle = supervisor.serve();

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
    let (_id, waiter) = handle.add_and_watch(spec).await?;
    println!("outcome: {:?}", waiter.wait().await?);

    handle.shutdown().await?;
    Ok(())
}
