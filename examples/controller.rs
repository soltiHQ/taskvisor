//! # Controller Example
//!
//! Shows how to use Controller with three admission policies:
//! - Queue: tasks run one by one
//! - Replace: cancels running task, starts new one
//! - DropIfRunning: ignores new tasks if slot is busy
//!
//! ## Run
//! ```bash
//! cargo run --example controller --features "controller"
//! ```

#[cfg(not(feature = "controller"))]
compile_error!("error");

use std::{sync::Arc, time::Duration};
use taskvisor::{
    BackoffPolicy, Config, ControllerConfig, ControllerSpec, RestartPolicy, Supervisor, TaskError,
    TaskFn, TaskRef, TaskSpec,
};
use tokio_util::sync::CancellationToken;

fn make_spec(name: &'static str, duration_ms: u64) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(name, move |ctx: CancellationToken| async move {
        println!("{:>6}[{name}] started", "");

        let start = tokio::time::Instant::now();
        let sleep = tokio::time::sleep(Duration::from_millis(duration_ms));

        tokio::pin!(sleep);
        tokio::select! {
            _ = &mut sleep => {
                println!("{:>6}[{name}] completed in {:?}", "", start.elapsed());
                Ok(())
            }
            _ = ctx.cancelled() => {
                println!("{:>6}[{name}] cancelled after {:?}", "", start.elapsed());
                Err(TaskError::Canceled)
            }
        }
    });
    TaskSpec::new(task, RestartPolicy::Never, BackoffPolicy::default(), None)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let sup = Supervisor::builder(Config::default())
        .with_controller(ControllerConfig::default())
        .build();

    let runner = Arc::clone(&sup);
    tokio::spawn(async move {
        let _ = runner.run(vec![]).await;
    });
    sup.wait_ready().await;

    // ============================================================
    // Demo -> Queue: Tasks execute one after another
    // ============================================================
    println!("Demo 1: Queue Policy");
    println!(" └► Submit 3 tasks with same name: they run sequentially");

    for i in 1..=3 {
        let spec = make_spec("job-in-queue", 800);
        sup.submit(ControllerSpec::queue(spec)).await?;
    }

    tokio::time::sleep(Duration::from_secs(4)).await;
    println!();

    // ============================================================
    // Demo -> Replace: New task cancels running one
    // ============================================================
    println!("Demo 2: Replace Policy");
    println!(" └► Submit task, wait 500ms, submit another: first gets cancelled");

    let task_1 = make_spec("job-replace", 6000);
    let task_2 = make_spec("job-replace", 500);

    sup.submit(ControllerSpec::replace(task_1)).await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    sup.submit(ControllerSpec::replace(task_2)).await?;

    tokio::time::sleep(Duration::from_secs(2)).await;
    println!();

    // ============================================================
    // Demo -> DropIfRunning: Ignores(skip) new tasks while busy
    // ============================================================
    println!("Demo 3: DropIfRunning Policy");
    println!(" └► Submit task & submit another while first is running: second is ignored");

    let task_1 = make_spec("job-drop-if-running", 1000);
    let task_2 = make_spec("job-drop-if-running", 10000);

    sup.submit(ControllerSpec::drop_if_running(task_1)).await?;
    tokio::time::sleep(Duration::from_millis(250)).await;
    sup.submit(ControllerSpec::drop_if_running(task_2)).await?;

    tokio::time::sleep(Duration::from_secs(2)).await;
    println!();

    println!("Done");
    Ok(())
}
