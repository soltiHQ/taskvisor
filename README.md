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
taskvisor = "0.0.4"
```

> Optional features:
>  - `logging` - enables the built-in [`LogWriter`], which prints events to stdout _(for demo and debug)_;
>  - `events` - exports [`Event`] and [`EventKind`] types at the crate root for direct use.

```toml
[dependencies]
taskvisor = { version = "0.0.4", features = ["logging", "events"] }
```

## 📝 Quick start
#### Minimal Example (No subscribers)
```toml
[dependencies]
taskvisor = "0.0.4"
```
```rust
//! The simplest possible taskvisor usage demonstrating:
//! - Task definition with proper cancellation handling
//! - Basic supervisor setup
//! - Graceful shutdown on Ctrl+C
//! - No subscribers

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

    let spec = TaskSpec::new(task, RestartPolicy::Always, BackoffPolicy::default(), None);
    supervisor.run(vec![spec]).await?;
    Ok(())
}
```

#### Minimal Example (Embedded subscriber)
```toml
[dependencies]
taskvisor = { version = "0.0.4", features = ["logging"] }
```
```rust
//! # Minimal Example
//!
//! The simplest possible taskvisor usage demonstrating:
//! - Task definition with proper cancellation handling
//! - Built-in LogWriter subscriber for observability
//! - Graceful shutdown on Ctrl+C

use std::time::Duration;
use tokio_util::sync::CancellationToken;
use taskvisor::{
    BackoffPolicy, Config, LogWriter, RestartPolicy, Supervisor, Subscribe, TaskError, TaskFn,
    TaskSpec,
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

    let spec = TaskSpec::new(task, RestartPolicy::Always, BackoffPolicy::default(), None);
    supervisor.run(vec![spec]).await?;
    Ok(())
}
```

### More Examples
Check out the [examples](./examples) directory for:
- `task_patterns.rs`: Different restart strategies and patterns
- `custom_subscriber.rs`: Building custom subscribers

```bash
    cargo run --example custom_subscriber --features=events
    cargo run --example task_patterns --features=logging
```

## 🤝 Contributing
We're open to any new ideas and contributions.  
Found a bug? Have an idea? We welcome pull requests and issues.
