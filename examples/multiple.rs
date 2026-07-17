//! # Multiple restart policies
//!
//! One supervisor can manage tasks with different lifecycles:
//!
//! | Task        | Policy             | Behavior                              |
//! |-------------|--------------------|---------------------------------------|
//! | `one-shot`  | never restart      | run once                              |
//! | `resilient` | restart on failure | fail twice, then succeed              |
//! | `recurring` | periodic           | wait 500 ms after each successful run |
//!
//! The resilient task uses exponential backoff and a retry budget.
//! Only failure-driven restarts consume that budget.
//! The supervisor also has a five-second grace period for shutdown.
//!
//! These task bodies are short. They do not use `TaskContext`.
//! A resident loop should observe cancellation as shown in `worker.rs`.
//!
//! Run with `cargo run --example multiple`, then press Ctrl+C.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use taskvisor::prelude::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // One-shot: runs once and exits
    let one_shot: TaskRef = TaskFn::arc("one-shot", |_ctx| async move {
        println!("[one-shot] doing work...");
        tokio::time::sleep(Duration::from_millis(200)).await;
        println!("[one-shot] done.");
        Ok(())
    });

    // Resilient: fails first 2 attempts, succeeds on 3rd
    let attempt = Arc::new(AtomicU32::new(0));
    let resilient: TaskRef = TaskFn::arc("resilient", move |_ctx| {
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

    // Recurring: repeats every 500ms until Ctrl+C
    let cycle = Arc::new(AtomicU32::new(0));
    let recurring: TaskRef = TaskFn::arc("recurring", move |_ctx| {
        let cycle = Arc::clone(&cycle);
        async move {
            let n = cycle.fetch_add(1, Ordering::Relaxed) + 1;
            println!("[recurring] cycle #{n}");
            tokio::time::sleep(Duration::from_millis(300)).await;
            Ok(())
        }
    });

    let specs = vec![
        TaskSpec::once(one_shot),
        TaskSpec::periodic(recurring, Duration::from_millis(500)),
        TaskSpec::restartable(resilient)
            .with_backoff(
                BackoffPolicy::exponential(Duration::from_millis(200))
                    .with_max(Duration::from_secs(5)),
            )
            .with_max_retries(NonZeroU32::new(3).unwrap()),
    ];

    let config = SupervisorConfig::default().with_grace(Duration::from_secs(5));
    let supervisor = Supervisor::new(config, vec![]);
    supervisor.run(specs).await?;

    Ok(())
}
