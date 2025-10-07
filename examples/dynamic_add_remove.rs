//! # Example: dynamic_add_remove
//!
//! Dynamically add and remove tasks at runtime via the `Supervisor`.
//!
//! Demonstrates how to:
//! - Start the `Supervisor` (on a background task) with an initial task set.
//! - From another async task (“controller”), add/remove tasks over time.
//! - Let `Supervisor::run()` return naturally once the registry drains.
//!
//! ## Flow
//! ```text
//! main()
//!   ├─► spawn Supervisor::run(initial_specs)
//!   │     ├─► publish(TaskAddRequested ...)
//!   │     ├─► Registry::spawn_listener()
//!   │     └─► TaskActor::run() … publishes lifecycle events
//!   │
//!   └─► controller task
//!         ├─► Supervisor.add_task(...)
//!         ├─► Supervisor.remove_task(...)
//!         └─► (repeat)
//!
//! When the registry becomes empty:
//!   Supervisor::run() completes → main() joins the task and exits.
//! ```
//!
//! ## Run
//! ```bash
//! cargo run --example dynamic_add_remove
//! ```

use std::{sync::Arc, time::Duration};
use taskvisor::{
    BackoffPolicy, Config, RestartPolicy, Subscribe, Supervisor, TaskError, TaskFn, TaskRef,
    TaskSpec,
};
use tokio_util::sync::CancellationToken;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1) Configure runtime
    let mut cfg = Config::default();
    cfg.grace = Duration::from_secs(5);
    cfg.bus_capacity = 256;

    // No subscribers here (keep demo focused)
    let subs: Vec<Arc<dyn Subscribe>> = Vec::new();

    // 2) Create supervisor and wrap in Arc so we can use it from multiple tasks
    let sup = Arc::new(Supervisor::new(cfg, subs));

    // 3) Define a simple “ticker” task factory
    fn ticker(name: &'static str, period: Duration) -> TaskSpec {
        let task: TaskRef = TaskFn::arc(name, move |ctx: CancellationToken| {
            let period = period;
            async move {
                // Cooperative loop: sleep in small steps and exit fast on cancel
                loop {
                    if ctx.is_cancelled() {
                        // Graceful exit (counted as TaskStopped)
                        return Ok::<(), TaskError>(());
                    }
                    println!("[{name}] tick");
                    tokio::time::sleep(period).await;
                }
            }
        });

        TaskSpec::new(
            task,
            RestartPolicy::Always,    // keep ticking until removed
            BackoffPolicy::default(), // irrelevant here (we don't fail)
            None,                     // no per-attempt timeout
        )
    }

    // 4) Define a short one-shot task (never restarts)
    fn oneshot(name: &'static str) -> TaskSpec {
        let name_str = name.to_string();

        let task: TaskRef = TaskFn::arc(name, move |_ctx: CancellationToken| {
            let name = name_str.clone();
            async move {
                println!("[{name}] one-shot work...");
                tokio::time::sleep(Duration::from_millis(400)).await;
                println!("[{name}] done");
                Ok::<(), TaskError>(())
            }
        });

        TaskSpec::new(
            task,
            RestartPolicy::Never,
            BackoffPolicy::default(),
            Some(Duration::from_secs(5)),
        )
    }

    // 5) Initial task set: only A
    let initial = vec![ticker("ticker-A", Duration::from_millis(500))];

    // 6) Run the supervisor in a background task and await it later
    let sup_run = {
        let sup = Arc::clone(&sup);
        tokio::spawn(async move {
            if let Err(e) = sup.run(initial).await {
                eprintln!("[supervisor] error: {e}");
            }
        })
    };

    // 7) Controller: dynamically add/remove tasks over time
    let controller = {
        let sup = Arc::clone(&sup);
        tokio::spawn(async move {
            // Give the registry a moment to spawn the initial actor
            tokio::time::sleep(Duration::from_secs(1)).await;

            // Add ticker-B
            println!("[controller] add ticker-B");
            let _ = sup.add_task(ticker("ticker-B", Duration::from_millis(300)));

            // After 2s: add oneshot
            tokio::time::sleep(Duration::from_secs(2)).await;
            println!("[controller] add oneshot");
            let _ = sup.add_task(oneshot("oneshot"));

            // After 2s: remove ticker-A
            tokio::time::sleep(Duration::from_secs(2)).await;
            println!("[controller] remove ticker-A");
            let _ = sup.remove_task("ticker-A");

            // After 2s: remove ticker-B (registry becomes empty soon after)
            tokio::time::sleep(Duration::from_secs(2)).await;
            println!("[controller] remove ticker-B");
            let _ = sup.remove_task("ticker-B");

            // Done: supervisor will exit once the registry drains
        })
    };

    // 8) Wait for both tasks (controller finishes first; supervisor exits when registry empties)
    let _ = controller.await;
    let _ = sup_run.await;

    println!("[main] finished: registry drained, supervisor exited.");
    Ok(())
}
