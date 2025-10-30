//! # Example: retry_with_backoff
//!
//! Demonstrates how [`Taskvisor`] automatically retries failed tasks
//! according to [`BackoffPolicy`] and [`RestartPolicy`].
//!
//! The task fails several times before succeeding, showing how backoff
//! delay and jitter are applied between retries.
//!
//! ## Flow
//! ```text
//! TaskActor::run()
//!   ├─► publish(TaskStarting, attempt=1)
//!   ├─► run_once() → Err("boom #1")
//!   ├─► publish(TaskFailed)
//!   ├─► publish(BackoffScheduled{delay=100ms})
//!   ├─► sleep(delay)
//!   ├─► retry → attempt=2
//!   │     ├─► publish(TaskStarting)
//!   │     ├─► run_once() → Err("boom #2")
//!   │     ├─► publish(TaskFailed)
//!   │     ├─► publish(BackoffScheduled{delay≈200ms})
//!   │     └─► sleep(delay)
//!   ├─► retry → attempt=3 → Ok(())
//!   ├─► publish(TaskStopped)
//!   └─► publish(ActorExhausted)
//! ```
//!
//! ## Run
//! ```bash
//! cargo run --example retry_with_backoff
//! ```

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use taskvisor::{
    BackoffPolicy, Config, JitterPolicy, RestartPolicy, Supervisor, TaskFn, TaskRef, TaskSpec,
};
use tokio_util::sync::CancellationToken;

static FAIL_COUNT: AtomicU64 = AtomicU64::new(0);

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Configure runtime (bus capacity = 100, 5s grace period)
    let mut cfg = Config::default();
    cfg.grace = Duration::from_secs(5);
    cfg.bus_capacity = 100;

    // 2. No subscribers for simplicity (you can attach LogWriter if feature enabled)
    let subs = Vec::new();

    // 3. Create supervisor
    let sup = Supervisor::new(cfg, subs);

    // 4. Define a task that fails 2 times before succeeding
    let flaky: TaskRef = TaskFn::arc("flaky", |ctx: CancellationToken| async move {
        let attempt = FAIL_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        println!("[flaky] attempt {attempt}");

        if ctx.is_cancelled() {
            println!("[flaky] cancelled");
            return Ok(());
        }

        if attempt <= 2 {
            println!("[flaky] simulated failure #{attempt}");
            Err(taskvisor::TaskError::Fail {
                reason: format!("boom #{attempt}"),
            })
        } else {
            println!("[flaky] success on attempt {attempt}");
            Ok(())
        }
    });

    // 5. Define backoff (exponential * jitter)
    let backoff = BackoffPolicy {
        first: Duration::from_millis(100),
        max: Duration::from_secs(2),
        factor: 2.0,
        jitter: JitterPolicy::Equal,
        success_delay: Some(Duration::from_secs(1)),
    };

    // 6. Create spec: retry on failure
    let spec = TaskSpec::new(flaky, RestartPolicy::OnFailure, backoff, None);

    // 7. Run supervisor with this task
    sup.run(vec![spec]).await?;

    println!("[main] done.");
    Ok(())
}
