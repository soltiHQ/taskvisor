//! # Runtime limits and task defaults
//!
//! Use `SupervisorBuilder` when several tasks share runtime limits or execution defaults.
//! Runtime configuration controls supervisor-wide resources.
//! `TaskDefaults` fills settings that a `TaskSpec` leaves inherited; an explicit per-task setting always wins.
//!
//! ```text
//! SupervisorConfig ──► SupervisorBuilder ──► Supervisor
//! TaskDefaults ──────► SupervisorBuilder
//!
//! TaskSpec::from_defaults ──► resolve inherited settings at registry admission
//! ```
//!
//! | Setting                     | What this example limits                               |
//! |-----------------------------|--------------------------------------------------------|
//! | `max_concurrent = 4`        | attempt futures running at the same time               |
//! | `max_registered_tasks = 64` | tasks holding registry capacity                        |
//! | `ownership_capacity = 128`  | accepted task and subscriber lifetimes through cleanup |
//!
//! These limits protect different runtime phases; one does not replace another.
//!
//! This example uses checked integer setters, a finite ownership limit, bounded retries, and a default attempt timeout.
//! The task overrides only its timeout, fails once, then succeeds.
//! `try_build` keeps configuration and startup-capacity failures typed.
//!
//! Run with `cargo run --example configuration`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use taskvisor::prelude::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = SupervisorConfig::default()
        .try_with_max_concurrent(4)?
        .try_with_max_registered_tasks(64)?
        .try_with_ownership_capacity(128)?
        .with_grace(Duration::from_secs(5));

    let defaults = TaskDefaults::default()
        .with_backoff(BackoffPolicy::constant(Duration::from_millis(100)))
        .with_timeout(Duration::from_secs(2))
        .try_with_max_retries(3)?;

    let attempts = Arc::new(AtomicU32::new(0));
    let task: TaskRef = TaskFn::arc(move |_ctx| {
        let attempts = Arc::clone(&attempts);
        async move {
            let attempt = attempts.fetch_add(1, Ordering::Relaxed) + 1;
            println!("[configured-job] attempt #{attempt}");
            if attempt == 1 {
                Err(TaskError::fail("temporary failure"))
            } else {
                Ok(())
            }
        }
    });

    let spec = TaskSpec::from_defaults("configured-job", task).with_timeout(Duration::from_secs(1));
    let supervisor = Supervisor::builder(runtime)
        .with_task_defaults(defaults)
        .try_build()?;

    supervisor.run(vec![spec]).await?;
    Ok(())
}
