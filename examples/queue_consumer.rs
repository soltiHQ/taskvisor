//! # Queue Consumer: Reconnect and Restart on Failure
//!
//! The most common resident-task shape: a consumer that reads from a broker (Kafka, Redis, SQS, ...) and must survive connection failures.
//!
//! This example uses an in-process channel as a mock broker.
//! The supervision pattern is the same for a real client.
//!
//! ## The pattern
//!
//! - The task body is one **connection session**: connect, then consume in a loop.
//! - A connection error returns `Err(TaskError::fail(...))`.
//!   The supervisor restarts the task with exponential backoff and jitter.
//!   No handwritten retry loop.
//! - Every await on the broker goes through `ctx.run_until_cancelled(...)`.
//!   On shutdown the consumer stops immediately, even while waiting for a message.
//!
//! ## What this shows
//!
//! - `TaskSpec::restartable` + `BackoffPolicy::exponential(..).with_jitter(..)` - the reconnect policy.
//! - `ctx.run_until_cancelled(rx.recv())` - graceful shutdown while blocked on the broker.
//! - Shared state (the receiver) captured with the clone-into-closure pattern.
//! - The task returns `Ok(())` when the backlog is drained; `OnFailure` stops on success.
//!
//! ## Run
//!
//! ```bash
//! cargo run --example queue_consumer
//! ```
//!
//! ## Next
//!
//! | Example                          | What it adds                             |
//! |----------------------------------|------------------------------------------|
//! | [`worker.rs`](worker.rs)         | The minimal long-running worker pattern  |
//! | [`subscriber.rs`](subscriber.rs) | Observe the retries with a subscriber    |

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use taskvisor::prelude::*;
use tokio::sync::{Mutex, mpsc};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Mock broker: a channel with a finite backlog.
    // Dropping the sender closes the "connection" after 8 messages.
    let (tx, rx) = mpsc::unbounded_channel::<String>();
    for i in 1..=8 {
        tx.send(format!("message-{i}"))?;
    }
    drop(tx);

    let rx = Arc::new(Mutex::new(rx));
    let attempts = Arc::new(AtomicU32::new(0));

    let consumer: TaskRef = TaskFn::arc("queue-consumer", {
        let rx = Arc::clone(&rx);
        let attempts = Arc::clone(&attempts);
        move |ctx| {
            let rx = Arc::clone(&rx);
            let attempts = Arc::clone(&attempts);
            async move {
                // 1) "Connect" to the broker.
                //    The first attempt fails to demonstrate restart + backoff.
                let attempt = attempts.fetch_add(1, Ordering::Relaxed) + 1;
                if attempt == 1 {
                    println!(
                        "[consumer] connect failed (simulated), supervisor retries with backoff"
                    );
                    return Err(TaskError::fail("connection refused"));
                }
                println!("[consumer] connected on attempt #{attempt}");

                // 2) Consume until the broker closes or shutdown starts.
                let mut rx = rx.lock().await;
                loop {
                    // `?` exits with TaskError::Canceled on shutdown (clean stop).
                    match ctx.run_until_cancelled(rx.recv()).await? {
                        Some(msg) => println!("[consumer] processed {msg}"),
                        None => {
                            println!("[consumer] backlog drained, done");
                            return Ok(());
                        }
                    }
                }
            }
        }
    });

    // Reconnect policy: 100ms, 200ms, 400ms, ... capped at 5s, with jitter.
    let spec = TaskSpec::restartable(consumer).with_backoff(
        BackoffPolicy::exponential(Duration::from_millis(100))
            .with_max(Duration::from_secs(5))
            .with_jitter(JitterPolicy::Equal),
    );

    let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
    sup.run(vec![spec]).await?;

    println!("Done.");
    Ok(())
}
