//! # Long-running worker with graceful shutdown
//!
//! Use this pattern for a resident loop that should stop with the supervisor.
//!
//! ```text
//! Ctrl+C ──► runtime shutdown ──► TaskContext cancellation
//!                                      ▼
//!                            drop-safe sleep ends ──► Canceled ──► stop
//! ```
//!
//! `run_until_cancelled` may wrap only a future that is safe to stop by dropping.
//! Tokio's sleep is safe for this pattern. Cancellation drops the sleep, returns
//! `TaskError::Canceled`, and gives the task a branch in which to release resources.
//!
//! Taskvisor requests a cooperative stop first.
//! After the configured grace period, it may commit `ForceAborted`.
//! Synchronous task code can remain physically active until it returns control to Tokio.
//!
//! `TaskSpec::restartable` retries retryable failures.
//! A clean return or `TaskError::Canceled` stops the worker.
//! This example stops cooperatively when Ctrl+C requests shutdown.
//!
//! `run_with_os_signals` installs process-wide signal handlers.
//! Use `run_until` when the surrounding application owns signal handling.
//!
//! Run with `cargo run --example graceful_worker`, then press Ctrl+C.

use std::time::Duration;

use taskvisor::prelude::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let worker: TaskRef = TaskFn::arc(|ctx| async move {
        let mut tick = 0u64;
        loop {
            match ctx
                .run_until_cancelled(tokio::time::sleep(Duration::from_millis(500)))
                .await
            {
                Ok(()) => {
                    tick += 1;
                    println!("[ticker] tick #{tick}");
                }
                Err(canceled) => {
                    println!("[ticker] shutting down after {tick} ticks");
                    return Err(canceled); // clean stop, not a failure
                }
            }
        }
    });

    let spec = TaskSpec::restartable("ticker", worker);

    let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    supervisor.run_with_os_signals(vec![spec]).await?;

    Ok(())
}
