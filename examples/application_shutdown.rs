//! # Application-owned shutdown
//!
//! Use `Supervisor::run_until` when the application already owns its shutdown signal.
//! The supplied future requests shutdown; Taskvisor then joins its bounded cleanup workflow.
//!
//! ```text
//! application shutdown future
//!              │ resolves
//!              ▼
//! Supervisor::run_until ──► cancel tasks ──► cooperative cleanup ──► return
//! ```
//!
//! This example uses a timer as the application signal and exits automatically.
//! A service can replace it with a channel, server shutdown future, or another application-owned trigger.
//! The worker observes `TaskContext`, releases its work, and returns `TaskError::Canceled`.
//!
//! Use `run_with_os_signals` instead when Taskvisor should install process signal handlers.
//!
//! Run with `cargo run --example application_shutdown`.

use std::time::Duration;

use taskvisor::prelude::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let worker: TaskRef = TaskFn::arc(|ctx| async move {
        let mut tick = 0u32;
        loop {
            match ctx
                .run_until_cancelled(tokio::time::sleep(Duration::from_millis(250)))
                .await
            {
                Ok(()) => {
                    tick += 1;
                    println!("[worker] tick #{tick}");
                }
                Err(canceled) => {
                    println!("[worker] application shutdown observed");
                    return Err(canceled);
                }
            }
        }
    });

    let application_shutdown = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        println!("[application] requesting shutdown");
    };

    let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    supervisor
        .run_until(
            vec![TaskSpec::restartable("worker", worker)],
            application_shutdown,
        )
        .await?;

    Ok(())
}
