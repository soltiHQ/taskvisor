[![Minimum Rust 1.75](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://rust-lang.org)
[![Crates.io](https://img.shields.io/crates/v/taskvisor.svg)](https://crates.io/crates/taskvisor)
[![Apache 2.0](https://img.shields.io/badge/license-Apache2.0-orange.svg)](./LICENSE)

# taskvisor
> Event-driven task orchestration library for Rust.

<div>
  <a href="https://docs.rs/taskvisor/latest/taskvisor/"><img alt="API Docs" src="https://img.shields.io/badge/API%20Docs-4d76ae?style=for-the-badge&logo=rust&logoColor=white"></a>
  <a href="./examples/"><img alt="Examples" src="https://img.shields.io/badge/Examples-2ea44f?style=for-the-badge&logo=github&logoColor=white"></a>
</div>

## 📖 Features
- observe all lifecycle events (start/stop/failure/backoff/timeout/shutdown);
- integrate with custom observers for logging, metrics, monitoring, ...;
- enforce global concurrency limits and graceful shutdown on signals;
- spawn supervised task actors with restart/backoff/timeout policies.

## 📦 Installation
#### Cargo.toml:
```toml
[dependencies]
taskvisor = "0.1"
```

> Optional features:
>  - `logging` - enables the built-in [`LoggerObserver`], which prints events to stdout _(for demo and debug)_;
>  - `events` - exports [`Event`] and [`EventKind`] types at the crate root for direct use.

```toml
[dependencies]
taskvisor = { version = "0.1", features = ["logging", "events"] }
```

## 📝 Quick start
This example shows how taskvisor handles task lifecycle: `execution`, `failure`, `restart with backoff`, and `graceful shutdown`.
```rust
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use taskvisor::{
    BackoffStrategy, 
    Config, 
    RestartPolicy, 
    Supervisor,
    TaskFn, 
    TaskRef, 
    TaskSpec, 
    LoggerObserver, 
    TaskError,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cfg = Config {
        grace: Duration::from_secs(5),
        ..Default::default()
    };

    let sup = Supervisor::new(cfg.clone(), LoggerObserver);

    // Simple task that occasionally fails to demonstrate restart behavior
    let demo_task: TaskRef = TaskFn::arc("demo", |ctx: CancellationToken| async move {
        if ctx.is_cancelled() {
            return Ok(());
        }
        println!("Task running...");
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Fail 30% of the time to show a restart mechanism
        if rand::random::<f32>() < 0.3 {
            return Err(TaskError::Fail {
                error: "Simulated failure".into()
            });
        }
        println!("Task completed successfully");
        Ok(())
    });

    let spec = TaskSpec::new(
        demo_task,
        RestartPolicy::Always,                // Restart on both success and failure
        BackoffStrategy {
            first: Duration::from_secs(1),
            max: Duration::from_secs(5),
            factor: 2.0,                      // Exponential: 1s, 2s, 4s, 5s (capped)
        },
        None,                                 // No timeout
    );

    // Run until Ctrl+C
    sup.run(vec![spec]).await?;
    Ok(())
}
```

#### What this example shows:
- how tasks are defined using `TaskFn::arc`
- how `RestartPolicy::Always` keeps the task running continuously
- how `BackoffStrategy` delays retries after failures
- how `LoggerObserver` prints events to see what's happening
- how Ctrl+C triggers graceful shutdown with a 5-second grace period

## 🤝 Contributing
We're open to any new ideas and contributions.  
Found a bug? Have an idea? We welcome pull requests and issues.
