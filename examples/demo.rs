use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use taskvisor::{
    BackoffPolicy, Config, LogWriter, RestartPolicy, Supervisor, TaskError, TaskFn, TaskRef,
    TaskSpec,
};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let mut cfg = Config::default();
    cfg.grace = Duration::from_secs(10);
    cfg.max_concurrent = 2;
    cfg.timeout = Duration::from_secs(5);

    let supervisor = Supervisor::new(cfg.clone(), LogWriter);

    let ticker: TaskRef = TaskFn::arc("ticker", |ctx: CancellationToken| async move {
        loop {
            if ctx.is_cancelled() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            println!("tick");
        }
    });

    let n = Arc::new(AtomicU64::new(0));
    let flaky: TaskRef = {
        let n = n.clone();
        Arc::new(TaskFn::new("flaky", move |ctx: CancellationToken| {
            let n = n.clone();
            async move {
                let this = n.fetch_add(1, Ordering::Relaxed) + 1;
                tokio::time::sleep(Duration::from_secs(2)).await;
                if ctx.is_cancelled() {
                    return Ok(());
                }
                if this % 3 == 0 {
                    Err(TaskError::Fail {
                        error: format!("transient fail #{this}"),
                    })
                } else {
                    println!("flaky ok #{this}");
                    Ok(())
                }
            }
        }))
    };

    let tasks = vec![
        TaskSpec {
            task: ticker.clone(),
            restart: RestartPolicy::Always,
            backoff: BackoffPolicy::default(),
            timeout: Some(Duration::from_secs(5)),
        },
        TaskSpec::from_task(flaky.clone(), &cfg),
    ];

    match supervisor.run(tasks).await {
        Ok(()) => println!("runtime stopped gracefully"),
        Err(e) => println!("runtime stopped with error: {e}"),
    }

    Ok(())
}
