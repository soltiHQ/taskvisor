//! # Example: Controller Pipeline

use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

use taskvisor::{
    BackoffPolicy, Config, RestartPolicy, Supervisor, TaskError, TaskFn, TaskRef, TaskSpec,
};

#[cfg(feature = "controller")]
use taskvisor::{controller::Admission, controller::ControllerConfig, controller::ControllerSpec};

/// One-shot task factory.
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

#[cfg(feature = "controller")]
fn make_producer() -> TaskSpec {
    let task: TaskRef = TaskFn::arc("producer", |ctx: CancellationToken| async move {
        // Give controller time to start
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Get supervisor handle (need to pass via closure)
        // For now, we'll submit via a shared channel or just demo the API

        println!("[producer] submitting build (Queue)");
        // sup.submit(ControllerSpec::new("build", Admission::Queue, make_task("build", 800))).await?;

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
    #[cfg(not(feature = "controller"))]
    {
        eprintln!("This example requires the 'controller' feature.");
        eprintln!("Run with: cargo run --example basic_controller --features controller");
        return Ok(());
    }

    #[cfg(feature = "controller")]
    {
        let mut cfg = Config::default();
        cfg.grace = Duration::from_secs(5);

        let sup = Supervisor::builder(cfg)
            .with_subscribers(vec![])
            .with_controller(ControllerConfig::default())
            .build();

        // Spawn supervisor in background
        let sup_clone = Arc::clone(&sup);
        let sup_task = tokio::spawn(async move {
            if let Err(e) = sup_clone.run(vec![]).await {
                eprintln!("[supervisor] error: {e}");
            }
        });

        // Submit tasks via controller
        tokio::time::sleep(Duration::from_millis(100)).await;

        println!("[main] submitting build (Queue)");
        sup.submit(ControllerSpec::new(
            "build",
            Admission::Queue,
            make_task("build", 800),
        ))
        .await?;

        println!("[main] submitting test (DropIfRunning)");
        sup.submit(ControllerSpec::new(
            "test",
            Admission::DropIfRunning,
            make_task("test", 600),
        ))
        .await?;

        println!("[main] submitting deploy (Replace)");
        sup.submit(ControllerSpec::new(
            "deploy",
            Admission::Replace,
            make_task("deploy", 1000),
        ))
        .await?;

        // Wait a bit for tasks to complete
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Shutdown
        drop(sup);
        let _ = sup_task.await;

        println!("[main] finished");
    }

    Ok(())
}
