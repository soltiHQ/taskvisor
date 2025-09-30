//! # Simple data processing pipeline
//!
//! Demonstrates basic taskvisor features:
//! - Restart policies in action (on errors)
//! - Channel-based communication
//! - Multiple cooperating tasks
//! - Graceful shutdown

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use taskvisor::{
    BackoffPolicy,
    Config,
    LogWriter,
    RestartPolicy,
    Supervisor,
    TaskError,
    TaskFn,
    TaskRef,
    TaskSpec,
};

/// Generates numbers every second
fn number_generator(tx: mpsc::Sender<u64>) -> TaskRef {
    let counter = Arc::new(AtomicU64::new(0));

    TaskFn::arc("generator", move |ctx: CancellationToken| {
        let tx = tx.clone();
        let counter = counter.clone();

        async move {
            loop {
                // Check cancellation
                if ctx.is_cancelled() {
                    println!("🔢 Generator: Shutting down");
                    return Ok(());
                }
                let num = counter.fetch_add(1, Ordering::Relaxed);
                println!("🔢 Generator: Sending {}", num);

                if tx.send(num).await.is_err() {
                    return Err(TaskError::Fatal {
                        error: "Channel closed".into(),
                    });
                }

                // Cancellable sleep
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {},
                    _ = ctx.cancelled() => {
                        println!("🔢 Generator: Cancelled");
                        return Ok(());
                    }
                }
            }
        }
    })
}

/// Processes numbers and occasionally fails
fn processor(rx: mpsc::Receiver<u64>, tx: mpsc::Sender<String>) -> TaskRef {
    let rx = Arc::new(tokio::sync::Mutex::new(rx));

    TaskFn::arc("processor", move |ctx: CancellationToken| {
        let rx = rx.clone();
        let tx = tx.clone();

        async move {
            loop {
                let num = {
                    let mut rx = rx.lock().await;
                    tokio::select! {
                        num = rx.recv() => num,
                        _ = ctx.cancelled() => {
                            println!("⚙️  Processor: Cancelled");
                            return Ok(());
                        }
                    }
                };
                match num {
                    Some(num) => {
                        println!("⚙️  Processor: Got {}", num);

                        // Fail on numbers divisible by 5 to demonstrate restart
                        if num % 5 == 0 && num != 0 {
                            return Err(TaskError::Fail {
                                error: format!("Cannot process {} (divisible by 5)", num),
                            });
                        }
                        // Process the number
                        let result = format!("Processed: {} -> {}", num, num * 2);

                        if tx.send(result).await.is_err() {
                            return Err(TaskError::Fatal {
                                error: "Output channel closed".into(),
                            });
                        }
                    }
                    None => {
                        println!("⚙️  Processor: Input closed");
                        return Ok(());
                    }
                }
            }
        }
    })
}

/// Prints results
fn printer(rx: mpsc::Receiver<String>) -> TaskRef {
    let rx = Arc::new(tokio::sync::Mutex::new(rx));

    TaskFn::arc("printer", move |ctx: CancellationToken| {
        let rx = rx.clone();

        async move {
            let mut count = 0;

            loop {
                let result = {
                    let mut rx = rx.lock().await;
                    tokio::select! {
                        result = rx.recv() => result,
                        _ = ctx.cancelled() => {
                            println!("📝 Printer: Cancelled after {} items", count);
                            return Ok(());
                        }
                    }
                };

                match result {
                    Some(result) => {
                        count += 1;
                        println!("📝 Printer: [{}] {}", count, result);
                    }
                    None => {
                        println!("📝 Printer: Channel closed after {} items", count);
                        return Ok(());
                    }
                }
            }
        }
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("🚀 Simple Pipeline Demo");
    println!("   Watch how the processor fails on multiples of 5 and restarts!");
    println!("   Press Ctrl+C to stop\n");

    // Create channels
    let (num_tx, num_rx) = mpsc::channel(5);
    let (result_tx, result_rx) = mpsc::channel(10);

    // Configure supervisor
    let mut config = Config::default();
    config.grace = Duration::from_secs(3);
    config.max_concurrent = 3;

    let supervisor = Supervisor::new(config, LogWriter);

    let tasks = vec![
        // Generator: always restart to keep producing
        TaskSpec::new(
            number_generator(num_tx),
            RestartPolicy::Always,
            BackoffPolicy::default(),
            None,
        ),

        // Processor: restart on failure with backoff
        TaskSpec::new(
            processor(num_rx, result_tx),
            RestartPolicy::OnFailure,
            BackoffPolicy {
                first: Duration::from_secs(2),
                max: Duration::from_secs(5),
                factor: 1.5,
            },
            None,
        ),

        // Printer: never restart
        TaskSpec::new(
            printer(result_rx),
            RestartPolicy::Never,
            BackoffPolicy::default(),
            None,
        ),
    ];
    match supervisor.run(tasks).await {
        Ok(()) => println!("\n✅ Pipeline completed successfully"),
        Err(e) => println!("\n⚠️  Pipeline stopped with error: {}", e),
    }
    Ok(())
}