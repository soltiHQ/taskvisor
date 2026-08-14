//! # Multiple restart policies
//!
//! Use one supervisor to manage tasks with different restart lifecycles.
//!
//! | Task        | Policy             | Behavior                              |
//! |-------------|--------------------|---------------------------------------|
//! | `one-shot`  | never restart      | run once                              |
//! | `resilient` | restart on failure | fail twice, then succeed              |
//! | `recurring` | periodic           | wait 500 ms after each successful run |
//!
//! `max_retries(3)` permits the first failed attempt and up to three retries in one failure streak.
//! The resilient task succeeds on attempt three after two backoffs.
//! The recurring task works for 300 ms, then waits 500 ms before its next attempt.
//!
//! The one-shot and resilient tasks end naturally.
//! The recurring task keeps the supervisor alive until Ctrl+C.
//! These short task bodies do not observe `TaskContext`, but their waits fit inside the configured
//! five-second shutdown grace.
//! A resident worker should observe cancellation as shown in `graceful_worker.rs`.
//!
//! `run_with_os_signals` installs process-wide signal handlers.
//! Use `run_until` when the surrounding application owns signal handling.
//!
//! Run with `cargo run --example restart_policies`, then press Ctrl+C.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use taskvisor::prelude::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // One-shot: runs once and exits
    let one_shot: TaskRef = TaskFn::arc(|_ctx| async move {
        println!("[one-shot] doing work...");
        tokio::time::sleep(Duration::from_millis(200)).await;
        println!("[one-shot] done.");
        Ok(())
    });

    // Resilient: fails first 2 attempts, succeeds on 3rd
    let attempt = Arc::new(AtomicU32::new(0));
    let resilient: TaskRef = TaskFn::arc(move |_ctx| {
        let attempt = Arc::clone(&attempt);
        async move {
            let n = attempt.fetch_add(1, Ordering::Relaxed) + 1;
            println!("[resilient] attempt #{n}");
            tokio::time::sleep(Duration::from_millis(100)).await;

            if n < 3 {
                Err(TaskError::fail(format!("attempt #{n} not ready yet")))
            } else {
                println!("[resilient] success on attempt #{n}!");
                Ok(())
            }
        }
    });

    // Recurring: waits 500ms after each successful run, until Ctrl+C
    let cycle = Arc::new(AtomicU32::new(0));
    let recurring: TaskRef = TaskFn::arc(move |_ctx| {
        let cycle = Arc::clone(&cycle);
        async move {
            let n = cycle.fetch_add(1, Ordering::Relaxed) + 1;
            println!("[recurring] cycle #{n}");
            tokio::time::sleep(Duration::from_millis(300)).await;
            Ok(())
        }
    });

    let specs = vec![
        TaskSpec::once("one-shot", one_shot),
        TaskSpec::periodic("recurring", recurring, Duration::from_millis(500)),
        TaskSpec::restartable("resilient", resilient)
            .with_backoff(
                BackoffPolicy::exponential(Duration::from_millis(200))
                    .with_max(Duration::from_secs(5)),
            )
            .with_max_retries(NonZeroU32::new(3).unwrap()),
    ];

    let config = SupervisorConfig::default().with_grace(Duration::from_secs(5));
    let supervisor = Supervisor::new(config, vec![]);
    supervisor.run_with_os_signals(specs).await?;

    Ok(())
}
