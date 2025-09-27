# taskvisor
> A lightweight, flexible task orchestration library for Rust with built-in supervision, restart policies, and graceful shutdown support.

[![Crates.io](https://img.shields.io/crates/v/taskvisor.svg)](https://crates.io/crates/taskvisor)
[![Docs.rs](https://docs.rs/taskvisor/badge.svg)](https://docs.rs/taskvisor)

## Architecture
```text
               ┌─────────────┐
               │  Supervisor │
               └──────┬──────┘
             owns (bus, cfg, obs)
                      │
       ┌──────────────┼──────────────┐
       ▼              ▼              ▼
   TaskSpec       TaskSpec ...    TaskSpec
       ▼              ▼              ▼
   TaskActor      TaskActor ...   TaskActor
       │              │              │
       └────────────emits────────────┘
                      │
                      ▼
                 Event Bus ◄── receives ── Observer
```

## Features
- Supervision Trees - Automatically restart failed tasks with configurable policies
- Graceful Shutdown - Handle OS signals (SIGINT/SIGTERM) with configurable grace periods
- Backoff Strategies - Exponential, constant, or custom backoff between retries
- Observability - Pluggable observer system for metrics, logging, and monitoring
- Concurrency Control - Global semaphore-based task limiting
- Timeout Management - Per-task execution timeouts with automatic cancellation

## Installation
```toml
[dependencies]
taskvisor = "0.1"

# Optional features
taskvisor = { version = "0.1", features = ["logging", "events"] }
```

## Quick start
```rust
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use taskvisor::{Config, Supervisor, TaskFn, TaskRef, RestartPolicy};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Configure the supervisor
    let mut config = Config::default();
    config.max_concurrent = 4;
    config.grace = Duration::from_secs(10);

    // Create a supervisor with a simple logger
    let supervisor = Supervisor::new(config, taskvisor::LoggerObserver);

    // Define a task
    let worker: TaskRef = TaskFn::arc("worker", |ctx: CancellationToken| async move {
        while !ctx.is_cancelled() {
            // Do work...
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Ok(())
    });

    // Run with automatic restart on failure
    let spec = TaskSpec::from_task(worker, &config);
    supervisor.run(vec![spec]).await?;
    
    Ok(())
}
```

## Concepts
### Tasks
Tasks are async functions that receive a `CancellationToken` for cooperative shutdown:
```rust
use taskvisor::{Task, TaskError};
use async_trait::async_trait;

struct DatabaseSync;

#[async_trait]
impl Task for DatabaseSync {
    fn name(&self) -> &str { 
        "db-sync" 
    }

    async fn run(&self, ctx: CancellationToken) -> Result<(), TaskError> {
        while !ctx.is_cancelled() {
            // Perform sync...
            if let Err(e) = sync_database().await {
                // Retryable error
                return Err(TaskError::Fail { 
                    error: e.to_string() 
                });
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        Ok(())
    }
}
```

### Restart Policies
Control when tasks should be restarted:
- `RestartPolicy::OnFailure` - Restart only on errors (default)
- `RestartPolicy::Always` - Restart unconditionally
- `RestartPolicy::Never` - Run once and stop

### Backoff Strategies
Configure delays between restart attempts:
```rust
use taskvisor::BackoffStrategy;

let backoff = BackoffStrategy {
    first: Duration::from_millis(100),
    max: Duration::from_secs(30),
    factor: 2.0,  // Exponential backoff
};
```

### Custom Observers
Integrate with your metrics and monitoring systems or smth else:
```rust
use taskvisor::{Observer, Event, EventKind};
use async_trait::async_trait;

struct MetricsObserver;

#[async_trait]
impl Observer for MetricsObserver {
    async fn on_event(&self, event: &Event) {
        match event.kind {
            EventKind::TaskFailed => {
                metrics::counter!("task.failures", 1);
            }
            EventKind::TaskStarting => {
                metrics::gauge!("tasks.active", 1);
            }
            _ => {}
        }
    }
}
```


## Advanced Usage
### Complex Task Specifications
```rust
let spec = TaskSpec::new(
    task,
    RestartPolicy::OnFailure,
    BackoffStrategy {
        first: Duration::from_secs(1),
        max: Duration::from_secs(60),
        factor: 1.5,
    },
    Some(Duration::from_secs(30)), // Task timeout
);
```

### Graceful Shutdown
The supervisor automatically handles OS signals and attempts graceful shutdown:
- Receives SIGINT/SIGTERM
- Cancels all task tokens
- Waits up to `Config::grace` duration
- Reports stuck tasks if grace period exceeded

## Examples
Check the [examples/](https://github.com/soltiHQ/taskvisor/tree/main/examples) directory

## Contributing
We welcome contributions! Please see our Contributing Guidelines.
