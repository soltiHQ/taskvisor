//! # Periodic
//!
//! A task that runs, completes, waits 2 seconds, and runs again: forever.
//!
//! ## What this shows
//!
//! - `TaskSpec::periodic(task, every)` - the supervisor waits `every` after each successful completion before spawning the next run.
//! - The task itself is short-lived (print and exit).
//!   The supervisor handles the scheduling loop: your task doesn't need its own `loop {}`.
//! - The `TaskContext` is unused here because the task completes instantly.
//!
//! ## What this is NOT
//!
//! The interval starts **after** the task completes.
//! This is not a wall-clock schedule: there is no "daily at 03:00", and drift accumulates over time.
//! For aligned schedules, compute the next occurrence yourself (e.g. with the `cron` crate) and sleep until it inside the task.
//!
//! ## How it differs from a loop inside the task
//!
//! You could write `loop { do_work(); sleep(2s); }` inside the task, but then:
//! - The supervisor sees one long-running task, not periodic completions.
//! - You lose per-attempt events (`TaskStarting`, `TaskStopped`) in subscribers.
//! - Backoff on failure doesn't apply (you'd have to handle it yourself).
//!
//! **With `RestartPolicy::Always`, each cycle is a separate attempt with full lifecycle observability.**
//!
//! ## Runtime flavor
//!
//! We use `current_thread` here because a single-threaded runtime is enough for examples and tests.
//!
//! *It can be used with `#[tokio::main]` (defaults to multi-thread): taskvisor works with both.*
//!
//!
//! ## Run
//! ```bash
//! cargo run --example periodic
//! # Press Ctrl+C to stop
//! ```
//!
//! ## Next
//!
//! | Example                          | What it adds                               |
//! |----------------------------------|--------------------------------------------|
//! | [`multiple.rs`](multiple.rs)     | Combine different policies                 |
//! | [`subscriber.rs`](subscriber.rs) | Observe lifecycle events with a subscriber |

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

    let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
    sup.run(vec![spec]).await?;

    Ok(())
}
