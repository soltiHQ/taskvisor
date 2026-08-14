//! # Queue consumer: retry a failed broker connection
//!
//! Use this pattern when one task attempt represents one broker connection session.
//! An in-process channel stands in for Kafka, Redis, SQS, or another client.
//!
//! ```text
//! attempt
//! ├── connect error ──► TaskError::fail ──► jittered backoff ──► new attempt
//! └── connected ──────► receive loop
//!                           ├── broker closes ──► Completed
//!                           └── cancellation ───► Canceled
//! ```
//!
//! The first connection fails.
//! `TaskSpec::restartable` retries it with exponential base delays and equal jitter.
//! Equal jitter chooses between half of each base delay and the full delay.
//! The inherited retry limit is unlimited.
//!
//! `run_until_cancelled` drops the pending receive when shutdown starts.
//! Apply this pattern only when a real client's receive and acknowledgement operations are safe
//! to stop at that point.
//!
//! This finite mock closes after eight messages.
//! A clean return stops the `OnFailure` task, the registry becomes empty, and `Supervisor::run` returns.
//! This command does not exercise an external shutdown signal.
//!
//! Run with `cargo run --example queue_consumer`.

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

    let consumer: TaskRef = TaskFn::arc({
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

    // Retry policy: base delays of 100ms, 200ms, 400ms, ... capped at 5s, with jitter.
    let spec = TaskSpec::restartable("queue-consumer", consumer).with_backoff(
        BackoffPolicy::exponential(Duration::from_millis(100))
            .with_max(Duration::from_secs(5))
            .with_jitter(JitterPolicy::Equal),
    );

    let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    supervisor.run(vec![spec]).await?;

    Ok(())
}
