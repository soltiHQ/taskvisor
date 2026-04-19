//! # Multiple Tasks
//!
//! Runs three tasks concurrently, each with a different restart strategy.
//!
//! This example shows how the same supervisor manages tasks with completely different lifecycles.
//!
//! ## Tasks
//!
//! | Task        | Policy                                   | Behavior                              |
//! |-------------|------------------------------------------|---------------------------------------|
//! | `one-shot`  | `RestartPolicy::Never`                   | Runs once, exits.                     |
//! | `resilient` | `OnFailure` + backoff + `max_retries(3)` | Fails twice, succeeds on 3rd attempt. |
//! | `always-on` | `Always { interval: 500ms }`             | Runs forever in a loop.               |
//!
//! ## What this shows
//!
//! - **`BackoffPolicy`** customization: exponential factor, initial delay, max cap.
//!   `resilient` uses `factor: 2.0` → delays: 200ms, 400ms, 800ms, …
//! - **`max_retries(3)`**: the task gives up after 3 failure-driven retries.
//!   Success-driven restarts (like `always-on`) don't count toward max_retries.
//! - **`SupervisorConfig::grace`**: how long the supervisor waits for tasks to stop during shutdown.
//!   Here set to 5 seconds.
//! - The `always-on` task runs indefinitely because `RestartPolicy::Always` never stops.
//!   The supervisor only exits when it receives Ctrl+C *(which triggers`drive_shutdown` → cancel all → grace period → exit)*.
//!
//! ## Why `_ctx` is unused
//!
//! These tasks are short-lived (100–300ms). They complete before any shutdown signal could arrive.
//! For long-lived tasks, always use `ctx.cancelled()` - see `worker.rs`.
//!
//! ## Runtime flavor
//!
//! We use `current_thread` here because a single-threaded runtime is enough for examples and tests.
//!
//! *It can be used with `#[tokio::main]` (defaults to multi-thread): taskvisor works with both.*
//!
//! ## Run
//!
//! ```bash
//! cargo run --example multiple
//! # Press Ctrl+C to stop (always-on task runs indefinitely)
//! ```
//!
//! ## Next
//!
//! | Example                    | What it adds                                       |
//! |----------------------------|----------------------------------------------------|
//! | [`metrics.rs`](metrics.rs) | Observe lifecycle events with a subscriber         |
//! | [`dynamic.rs`](dynamic.rs) | Add/remove tasks at runtime via `SupervisorHandle` |

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use taskvisor::prelude::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // One-shot: runs once and exits
    let one_shot: TaskRef = TaskFn::arc("one-shot", |_ctx: CancellationToken| async move {
        println!("[one-shot] doing work...");
        tokio::time::sleep(Duration::from_millis(200)).await;
        println!("[one-shot] done.");
        Ok(())
    });

    // Resilient: fails first 2 attempts, succeeds on 3rd
    let attempt = Arc::new(AtomicU32::new(0));
    let resilient: TaskRef = TaskFn::arc("resilient", move |_ctx: CancellationToken| {
        let attempt = Arc::clone(&attempt);
        async move {
            let n = attempt.fetch_add(1, Ordering::Relaxed) + 1;
            println!("[resilient] attempt #{n}");
            tokio::time::sleep(Duration::from_millis(100)).await;

            if n < 3 {
                Err(TaskError::Fail {
                    reason: format!("attempt #{n} not ready yet"),
                    exit_code: None,
                })
            } else {
                println!("[resilient] success on attempt #{n}!");
                Ok(())
            }
        }
    });

    // Always-on: repeats every 500ms until Ctrl+C
    let cycle = Arc::new(AtomicU32::new(0));
    let always_on: TaskRef = TaskFn::arc("always-on", move |_ctx: CancellationToken| {
        let cycle = Arc::clone(&cycle);
        async move {
            let n = cycle.fetch_add(1, Ordering::Relaxed) + 1;
            println!("[always-on] cycle #{n}");
            tokio::time::sleep(Duration::from_millis(300)).await;
            Ok(())
        }
    });

    let specs = vec![
        TaskSpec::once(one_shot),
        TaskSpec::once(always_on).with_restart(RestartPolicy::Always {
            interval: Some(Duration::from_millis(500)),
        }),
        TaskSpec::restartable(resilient)
            .with_backoff(BackoffPolicy {
                first: Duration::from_millis(200),
                max: Duration::from_secs(5),
                factor: 2.0,
                ..BackoffPolicy::default()
            })
            .with_max_retries(3),
    ];

    let cfg = SupervisorConfig {
        grace: Duration::from_secs(5),
        ..SupervisorConfig::default()
    };
    let sup = Supervisor::new(cfg, vec![]);
    sup.run(specs).await?;

    println!("All tasks finished.");
    Ok(())
}
