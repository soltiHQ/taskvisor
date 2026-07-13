//! # Long-running worker with graceful shutdown
//!
//! This is the basic pattern for a task that runs until shutdown. Each blocking
//! await is wrapped with `TaskContext::run_until_cancelled`, so cancellation can
//! stop the worker without waiting for the operation to finish.
//!
//! Taskvisor first asks tasks to stop cooperatively. A task that does not observe
//! cancellation may be force-aborted after the configured grace period. Observe
//! the context so the task can release resources and finish cleanly.
//!
//! `TaskSpec::restartable` uses `RestartPolicy::OnFailure`: failures restart the
//! worker, while a clean return stops it. `TaskError::Canceled` is a clean stop.
//!
//! Run with `cargo run --example worker`, then press Ctrl+C.

use std::time::Duration;

use taskvisor::prelude::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let worker: TaskRef = TaskFn::arc("ticker", |ctx| async move {
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

    let spec = TaskSpec::restartable(worker);

    let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
    sup.run(vec![spec]).await?;

    Ok(())
}
