//! # Example: basic_one_shot
//!
//! Minimal example of a single one-shot task without retries, backoff, or subscribers.
//!
//! Demonstrates how to:
//! - Define a simple task using [`TaskFn`].
//! - Wrap it in a [`TaskSpec`] with `RestartPolicy::Never`.
//! - Run it under [`Supervisor`] and exit cleanly.
//!
//! ## Flow
//! ```text
//! TaskSpec ──► Supervisor::run()
//!     ├─► Bus.publish(TaskAddRequested)
//!     ├─► Registry::spawn_listener()
//!     ├─► TaskActor::run()
//!     │     ├─► publish(TaskStarting)
//!     │     ├─► run_once()
//!     │     ├─► publish(TaskStopped)
//!     │     └─► publish(ActorExhausted)
//!     └─► Supervisor::drive_shutdown()
//!          ├─► publish(AllStoppedWithin)
//!          └─► exit
//! ```
//!
//! ## Run
//! ```bash
//! cargo run --example basic_one_shot
//! ```

use std::time::Duration;
use taskvisor::{BackoffPolicy, Config, RestartPolicy, Supervisor, TaskFn, TaskRef, TaskSpec};
use tokio_util::sync::CancellationToken;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Build runtime configuration (defaults are fine for one-shot)
    let cfg = Config::default();

    // 2. No subscribers for simplicity
    let subs = Vec::new();

    // 3. Create Supervisor
    let sup = Supervisor::new(cfg, subs);

    // 4. Define a simple async task
    let hello: TaskRef = TaskFn::arc("hello", |ctx: CancellationToken| async move {
        println!("[hello] started");
        for i in 1..=3 {
            if ctx.is_cancelled() {
                println!("[hello] cancelled");
                return Ok(());
            }
            println!("[hello] tick {i}");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        println!("[hello] done");
        Ok(())
    });

    // 5. Build a one-shot spec (never retry)
    let spec = TaskSpec::new(
        hello,
        RestartPolicy::Never,
        BackoffPolicy::default(),
        Some(Duration::from_secs(5)),
    );

    // 6. Run supervisor with this task
    sup.run(vec![spec]).await?;
    Ok(())
}
