//! # Periodic task
//!
//! Use `TaskSpec::periodic` for short work that should repeat after each success.
//!
//! ```text
//! attempt
//! ├── success ────────────► wait `every` ─────► next attempt
//! ├── retryable failure ──► failure backoff ──► retry
//! └── fatal or canceled ──► stop
//! ```
//!
//! Every invocation is a separate attempt with its own lifecycle events.
//! The success interval begins after the task finishes.
//! Work duration is added to the time between starts.
//!
//! This is fixed-delay scheduling, not a wall-clock or cron schedule.
//!
//! This heartbeat always succeeds and repeats until Ctrl+C.
//! `run_with_os_signals` installs process-wide signal handlers.
//! Use `run_until` when the application owns them.
//!
//! Run with `cargo run --example periodic`, then press Ctrl+C.

use std::time::Duration;

use taskvisor::prelude::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let heartbeat: TaskRef = TaskFn::arc(|_ctx| async move {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        println!("[heartbeat] ping at {:.0}s", now.as_secs_f64());
        Ok(())
    });

    let spec = TaskSpec::periodic("heartbeat", heartbeat, Duration::from_secs(2));

    let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    supervisor.run_with_os_signals(vec![spec]).await?;

    Ok(())
}
