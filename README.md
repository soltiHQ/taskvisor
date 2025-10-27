[![Minimum Rust 1.85](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://rust-lang.org)
[![Crates.io](https://img.shields.io/crates/v/taskvisor.svg)](https://crates.io/crates/taskvisor)
[![Apache 2.0](https://img.shields.io/badge/license-Apache2.0-orange.svg)](./LICENSE)

# taskvisor
> Event-driven task orchestration library for Rust.

<div>
  <a href="https://docs.rs/taskvisor/latest/taskvisor/"><img alt="API Docs" src="https://img.shields.io/badge/API%20Docs-4d76ae?style=for-the-badge&logo=rust&logoColor=white"></a>
  <a href="./examples/"><img alt="Examples" src="https://img.shields.io/badge/Examples-2ea44f?style=for-the-badge&logo=github&logoColor=white"></a>
</div>

## 📖 Features
- Observe lifecycle events (start / stop / failure / backoff / timeout / shutdown)
- Plug custom subscribers for logging, metrics, alerting
- Global concurrency limiting and graceful shutdown on OS signals
- Supervised task actors with restart/backoff/timeout policies

## 📦 Installation
#### Cargo.toml:
```toml
[dependencies]
taskvisor = "0.0.7"
```

> Optional features:
>  - `logging` enables the built-in [`LogWriter`], (demo logger);
>  - `controller` enables the slot-based [`Controller`] with admission policies.

```toml
[dependencies]
taskvisor = { version = "0.0.8", features = ["logging"] }
```

## 📝 Quick start
#### Minimal Example (No subscribers)
```toml
[dependencies]
taskvisor = "0.0.8"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "time", "sync", "signal"] }
tokio-util = { version = "0.7", features = ["rt"] }
anyhow = "1"
```
```rust
//! Minimal: single task, no subscribers.

use std::time::Duration;
use tokio_util::sync::CancellationToken;
use taskvisor::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let supervisor = Supervisor::new(Config::default(), vec![]);
    
    let task = TaskFn::arc("hello", |ctx: CancellationToken| async move {
        if ctx.is_cancelled() {
            return Err(TaskError::Canceled);
        }

        println!("Hello from taskvisor!");
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(())
    });

    let spec = TaskSpec::new(
        task, 
        RestartPolicy::Always, 
        BackoffPolicy::default(), 
        None,
    );
    supervisor.run(vec![spec]).await?;
    Ok(())
}
```

#### Minimal Example (Embedded subscriber)
```toml
[dependencies]
taskvisor = { version = "0.0.8", features = ["logging"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread", "time", "sync", "signal"] }
tokio-util = { version = "0.7", features = ["rt"] }
anyhow = "1"
```
```rust
//! Minimal with built-in LogWriter subscriber.

use std::time::Duration;
use tokio_util::sync::CancellationToken;
use taskvisor::{
    BackoffPolicy, Config, LogWriter, RestartPolicy, Supervisor, 
    Subscribe, TaskError, TaskFn, TaskSpec,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Setup supervisor with LogWriter to see task lifecycle events
    let subscribers: Vec<std::sync::Arc<dyn Subscribe>> = vec![
        std::sync::Arc::new(LogWriter),
    ];
    let supervisor = Supervisor::new(Config::default(), subscribers);
    
    let task = TaskFn::arc("hello", |ctx: CancellationToken| async move {
        if ctx.is_cancelled() {
            return Err(TaskError::Canceled);
        }

        println!("Hello from taskvisor!");
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(())
    });

    let spec = TaskSpec::new(
        task, 
        RestartPolicy::Always, 
        BackoffPolicy::default(), 
        None,
    );
    supervisor.run(vec![spec]).await?;
    Ok(())
}
```

#### Dynamic Tasks Example
```toml
[dependencies]
taskvisor = "0.0.8"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "time", "sync", "signal"] }
tokio-util = { version = "0.7", features = ["rt"] }
anyhow = "1"
```
```rust
//! Demonstrates how a running task can add another task dynamically.

use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use taskvisor::{
    BackoffPolicy, Config, RestartPolicy, Supervisor, TaskError, 
    TaskFn, TaskRef, TaskSpec,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let supervisor = Arc::new(Supervisor::new(Config::default(), vec![]));
    let sup = supervisor.clone();

    let controller: TaskRef = TaskFn::arc("controller", move |ctx: CancellationToken| {
        let sup = sup.clone();
        async move {
            println!("[controller] preparing to start worker...");
            tokio::time::sleep(Duration::from_millis(500)).await;

            let worker: TaskRef = TaskFn::arc("worker", |_ctx| async move {
                println!("[worker] started");
                tokio::time::sleep(Duration::from_millis(400)).await;
                println!("[worker] done");
                Ok::<(), TaskError>(())
            });

            let worker_spec = TaskSpec::new(
                worker,
                RestartPolicy::Never,
                BackoffPolicy::default(),
                Some(Duration::from_secs(5)),
            );

            // Publish TaskAddRequested (handled by Registry)
            let _ = sup.add_task(worker_spec);

            println!("[controller] worker task requested!");
            Ok(())
        }
    });

    let spec = TaskSpec::new(
        controller,
        RestartPolicy::Never,
        BackoffPolicy::default(),
        None,
    );

    supervisor.run(vec![spec]).await?;
    Ok(())
}
```

### Controller Feature
When the controller feature is enabled, Taskvisor gains a dedicated Controller layer 
that manages task admission and scheduling before tasks are handed to the Supervisor.

```toml
# Cargo.toml
[dependencies]
taskvisor = { version = "0.0.8", features = ["controller"] }
```

```text
submit(ControllerSpec)
          ▼
   ┌──────────────┐
   │  Controller  │  ← Admission control (Drop / Replace / Queue)
   └──────┬───────┘
          ▼
   ┌──────────────┐
   │  Supervisor  │
   │  spawns task │
   └──────┬───────┘
          ▼
      Task Actors
```

```rust
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use taskvisor::{
    Supervisor, ControllerConfig, ControllerSpec,
    TaskFn, TaskSpec, RestartPolicy, BackoffPolicy, TaskError,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let sup = Supervisor::builder(Default::default())
        .with_controller(ControllerConfig::new())
        .build();

    let sup_bg = Arc::clone(&sup);
    tokio::spawn(async move { let _ = sup_bg.run(vec![]).await; });

    let spec = TaskSpec::new(
        TaskFn::arc("job", |ctx: CancellationToken| async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            if ctx.is_cancelled() { return Ok(()); }
            Ok::<(), TaskError>(())
        }),
        RestartPolicy::Never,
        BackoffPolicy::default(),
        None,
    );

    sup.submit(ControllerSpec::queue(spec.clone())).await?;
    sup.submit(ControllerSpec::queue(spec)).await?;

    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}
```

#### Admission modes:
- `DropIfRunning` discard if a task with the same name is still active.
- `Replace` cancel and replace the running task.
- `Queue` enqueue until the slot becomes free.
__The controller uses an internal async queue (mpsc) to serialize submissions and integrates with the global concurrency limit of Supervisor.__

### More Examples
Check out the [examples](./examples) directory for:
- [basic_one_shot.rs](examples/basic_one_shot.rs): single one-shot task, graceful shutdown
- [retry_with_backoff.rs](examples/retry_with_backoff.rs): retry loop with exponential backoff and jitter
- [dynamic_add_remove.rs](examples/dynamic_add_remove.rs): add/remove tasks at runtime via API
- [custom_subscriber.rs](examples/custom_subscriber.rs): custom subscriber reacting to events
- [task_cancel.rs](examples/task_cancel.rs): task cancellation from outside
- [controller.rs](examples/controller.rs): examples with `controller` feature

```bash
# basic / retry / dynamic do not require extra features
cargo run --example basic_one_shot
cargo run --example retry_with_backoff
cargo run --example dynamic_add_remove
cargo run --example custom_subscriber
cargo run --example task_cancel --features logging
cargo run --example controller --features controller
```

## 🤝 Contributing
We're open to any new ideas and contributions.  
Found a bug? Have an idea? We welcome pull requests and issues.
