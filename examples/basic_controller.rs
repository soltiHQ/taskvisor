//! # Example: Controller Pipeline
//!
//! Demonstrates how to use the controller to coordinate multiple tasks
//! with different admission policies.
//!
//! ```text
//!  producer (5s timeout)
//!   ├──► submit(build) Queue
//!   ├──► submit(test) DropIfRunning
//!   └──► submit(deploy) Replace
//!
//! Controller decides per-slot behavior:
//! - build → queued
//! - test  → ignored if busy
//! - deploy → replaces current
//! ```

use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

use taskvisor::{
    BackoffPolicy, Config, RestartPolicy, Subscribe, Supervisor, TaskError, TaskFn, TaskRef, TaskSpec,
    controller::task::{controller, ControllerConfig, bind_supervisor, submit},
    controller::admission::Admission,
    controller::ControllerSpec,
};

/// One-shot task factory (no loops).
fn make_task(name: &'static str, work_ms: u64) -> TaskSpec {
    let n = name.to_string();
    let task: TaskRef = TaskFn::arc(name, move |_ctx: CancellationToken| {
        let n = n.clone();
        async move {
            println!("[{n}] start (work {work_ms}ms)");
            tokio::time::sleep(Duration::from_millis(work_ms)).await;
            println!("[{n}] done");
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

/// Producer task — sends 3 jobs into the controller with different admissions.
fn make_producer() -> TaskSpec {
    let task: TaskRef = TaskFn::arc("producer", |_ctx: CancellationToken| async move {
        println!("[producer] submitting build (Queue)");
        submit(ControllerSpec::new(Admission::Queue, make_task("build", 800)))
            .await
            .expect("submit build");

        println!("[producer] submitting test (DropIfRunning)");
        submit(ControllerSpec::new(Admission::DropIfRunning, make_task("test", 600)))
            .await
            .expect("submit test");

        println!("[producer] submitting deploy (Replace)");
        submit(ControllerSpec::new(Admission::Replace, make_task("deploy", 1000)))
            .await
            .expect("submit deploy");

        println!("[producer] done");
        Ok::<(), TaskError>(())
    });

    TaskSpec::new(
        task,
        RestartPolicy::Never,
        BackoffPolicy::default(),
        Some(Duration::from_secs(5)),
    )
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // 1) runtime
    let mut cfg = Config::default();
    cfg.grace = Duration::from_secs(5);
    cfg.bus_capacity = 256;

    let subs: Vec<Arc<dyn Subscribe>> = Vec::new();
    let sup = Arc::new(Supervisor::new(cfg, subs));

    // 2) bind controller to supervisor
    bind_supervisor(Arc::clone(&sup));

    // 3) tasks
    let ctrl = controller(ControllerConfig::default());
    let producer = make_producer();

    // 4) run supervisor
    if let Err(e) = sup.run(vec![ctrl, producer]).await {
        eprintln!("[supervisor] error: {e}");
    }

    println!("[main] finished");
    Ok(())
}