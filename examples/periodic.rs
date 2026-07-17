//! # Periodic task
//!
//! `TaskSpec::periodic` runs a short task again after each completion.
//! Each run is a separate attempt.
//! It has its own lifecycle events.
//! Failures use the configured retry and backoff rules.
//!
//! The interval starts after the task finishes.
//! This is a fixed-delay schedule, not a wall-clock or cron schedule.
//! Work duration adds to the time between starts. Drift is expected.
//!
//! Run with `cargo run --example periodic`, then press Ctrl+C.

use std::time::Duration;

use taskvisor::prelude::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let heartbeat: TaskRef = TaskFn::arc("heartbeat", |_ctx| async move {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        println!("[heartbeat] ping at {:.0}s", now.as_secs_f64());
        Ok(())
    });

    let spec = TaskSpec::periodic(heartbeat, Duration::from_secs(2));

    let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    supervisor.run(vec![spec]).await?;

    Ok(())
}
