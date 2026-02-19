[![Minimum Rust 1.85](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://rust-lang.org)
[![Crates.io](https://img.shields.io/crates/v/taskvisor.svg)](https://crates.io/crates/taskvisor)
[![Apache 2.0](https://img.shields.io/badge/license-Apache2.0-orange.svg)](./LICENSE)
#### Install
```toml
[dependencies]
taskvisor = "0.0"
```
#### With features:
```toml
[dependencies]
taskvisor = { version = "0.0", features = ["controller", "logging"] }
```
See [crates.io](https://crates.io/crates/taskvisor) for the latest version.

# Taskvisor
> Lightweight, event-driven task supervision for async Rust.

A small orchestrator that runs your background tasks, restarts them on failure, and tells you what happened.
```text
 ┌────────────┐     runs & restarts    ┌──────────────┐
 │   Tasks    │ <───────────────────── │  Supervisor  │
 └────────────┘                        └──────┬───────┘
                                              |
                                         emits events
                                              |
                                       ┌──────────────┐
                                       │ Subscribers  │ <─ your metrics / logs
                                       └──────────────┘
```
Use it for long-lived jobs, background workers, or anything that must stay alive, observable, and restart safely.

<div>
  <a href="https://docs.rs/taskvisor/latest/taskvisor/"><img alt="API Docs" src="https://img.shields.io/badge/API%20Docs-4d76ae?style=for-the-badge&logo=rust&logoColor=white"></a>
  <a href="./examples/"><img alt="Examples" src="https://img.shields.io/badge/Examples-2ea44f?style=for-the-badge&logo=github&logoColor=white"></a>
</div>

---

## Quick start

A task that prints "pong" every 10 seconds, restarts forever, and shuts down on Ctrl+C:
```rust
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use taskvisor::{
    Supervisor, SupervisorConfig, TaskFn, TaskSpec, TaskRef,
    TaskError, RestartPolicy, BackoffPolicy,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sup = Supervisor::new(SupervisorConfig::default(), vec![]);

    let ping: TaskRef = TaskFn::arc("ping", |ctx: CancellationToken| async move {
        tokio::select! {
            _ = ctx.cancelled() => return Err(TaskError::Canceled),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        println!("[ping] pong");
        Ok(())
    });

    let spec = TaskSpec::new(
        ping,
        RestartPolicy::Always { interval: Some(Duration::from_secs(10)) },
        BackoffPolicy::default(),
        None, // no timeout
    );

    sup.run(vec![spec]).await?;
    Ok(())
}
```

---

## Core concepts

**Task & TaskFn** — A `Task` is any `Send + Sync + 'static` type that implements `fn spawn(&self, ctx: CancellationToken) -> BoxTaskFuture`. `TaskFn` wraps a closure into a `Task` so you don't need a struct. `TaskRef` is just `Arc<dyn Task>`.

**TaskSpec** — Bundles a task with its policies: restart, backoff, and optional timeout. This is what you pass to the supervisor.

**RestartPolicy** — Controls when a task restarts after it exits:

| Policy | Behavior |
|--------|----------|
| `Never` | Run once, done. |
| `OnFailure` *(default)* | Restart only on error. Success = stop. |
| `Always { interval }` | Always restart. `interval: Some(10s)` waits between successes. |

**BackoffPolicy** — Controls retry delay after failure. Delay for attempt `n` = `first * factor^n`, capped at `max`, then jitter is applied:

| Field | Default | Meaning |
|-------|---------|---------|
| `first` | `100ms` | Initial delay |
| `max` | `30s` | Delay cap |
| `factor` | `1.0` | Multiplier per attempt (`2.0` = exponential) |
| `jitter` | `None` | Randomization strategy (see below) |

**JitterPolicy** — Prevents thundering herd when multiple tasks retry at the same time:

| Policy | Range | Use when |
|--------|-------|----------|
| `None` | exact delay | Single task, predictable timing |
| `Full` | `[0, delay]` | Maximum spread needed |
| `Equal` *(recommended)* | `[delay/2, delay]` | Balanced: preserves ~75% of backoff |
| `Decorrelated` | `[base, base*3]` capped at `max` | Sophisticated, self-adjusting |

**Supervisor** — Owns the runtime. Spawns task actors, distributes events to subscribers, handles shutdown.

**Events & Subscribe** — Every lifecycle change (start, stop, fail, timeout, backoff, shutdown) is published to a broadcast bus. Implement `Subscribe` to observe them. Each subscriber gets its own queue — a slow subscriber never blocks others.

---

## Recipes

### Exponential backoff with jitter
```rust
use std::time::Duration;
use taskvisor::{BackoffPolicy, JitterPolicy};

let backoff = BackoffPolicy {
    first: Duration::from_millis(200),
    max: Duration::from_secs(30),
    factor: 2.0,                    // 200ms -> 400ms -> 800ms -> ...
    jitter: JitterPolicy::Equal,    // recommended: [delay/2, delay]
};
```

### Per-task timeout
```rust
// Task gets 5s per attempt. If exceeded: TimeoutHit event + TaskError::Timeout + retry.
let spec = TaskSpec::new(task, RestartPolicy::OnFailure, backoff, Some(Duration::from_secs(5)));
```

### Custom subscriber (metrics)
```rust
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use async_trait::async_trait;
use taskvisor::{Subscribe, Event, EventKind};

struct Metrics { failures: AtomicU64 }

#[async_trait]
impl Subscribe for Metrics {
    async fn on_event(&self, event: &Event) {
        if matches!(event.kind, EventKind::TaskFailed) {
            self.failures.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// Pass to supervisor:
let metrics = Arc::new(Metrics { failures: AtomicU64::new(0) });
let sup = Supervisor::new(config, vec![metrics]);
```

### Periodic task (run every N seconds)
```rust
// Runs, completes, waits 30s, runs again. Forever.
let spec = TaskSpec::new(
    task,
    RestartPolicy::Always { interval: Some(Duration::from_secs(30)) },
    BackoffPolicy::default(), // backoff only applies on failure
    None,
);
```

### Runtime task management
```rust
// Add/remove/cancel while running
sup.add_task(spec).await?;
sup.cancel("task-name").await?;
sup.remove_task("task-name").await?;
let alive = sup.is_alive("task-name").await;
let tasks = sup.list_tasks().await;
```

---

## Error handling

Return these from your task to control what happens next:

| Return | Retryable | What happens |
|--------|-----------|-------------|
| `Ok(())` | - | Task completed. `RestartPolicy` decides next step. |
| `Err(TaskError::Fail { reason })` | Yes | Retryable failure. Backoff, then retry. |
| `Err(TaskError::Timeout { .. })` | Yes | Set automatically when per-task timeout is exceeded. |
| `Err(TaskError::Fatal { reason })` | No | Permanent failure. Actor stops, publishes `ActorDead`. |
| `Err(TaskError::Canceled)` | No | Graceful shutdown. Not an error. |

### Cancellation pattern

Tasks **must** observe cancellation. Two equivalent patterns:
```rust
// Pattern 1: select! (recommended for long-running tasks)
tokio::select! {
    _ = ctx.cancelled() => Err(TaskError::Canceled),
    result = do_work() => result,
}

// Pattern 2: check before work (ok for short tasks)
if ctx.is_cancelled() { return Err(TaskError::Canceled); }
do_work().await
```

---

## Events

Every lifecycle change publishes an `Event` to the bus. Subscribe to observe them.

| Event | Meaning |
|-------|---------|
| **Lifecycle** | |
| `TaskStarting` | Attempt is beginning |
| `TaskStopped` | Completed or cancelled gracefully |
| `TaskFailed` | Attempt failed (retryable or fatal) |
| `TimeoutHit` | Attempt exceeded its timeout |
| `BackoffScheduled` | Retry delay scheduled (includes delay duration) |
| **Terminal** | |
| `ActorExhausted` | Restart policy says stop (normal end-of-life) |
| `ActorDead` | Fatal error, actor will not restart |
| **Management** | |
| `TaskAdded` / `TaskRemoved` | Task registered / unregistered |
| `TaskAddRequested` / `TaskRemoveRequested` | Add/remove commands received |
| **Shutdown** | |
| `ShutdownRequested` | OS signal caught (SIGTERM/SIGINT) |
| `AllStoppedWithinGrace` | Clean shutdown |
| `GraceExceeded` | Some tasks didn't stop in time |
| **Subscriber** | |
| `SubscriberPanicked` | A subscriber panicked (isolated, others unaffected) |
| `SubscriberOverflow` | Subscriber queue full, event dropped |

Each event carries: `kind`, `task` (name), `attempt`, `reason`, `delay_ms`, `timeout_ms`, `seq` (monotonic ordering), `at` (timestamp).

---

## Configuration

```rust
use taskvisor::SupervisorConfig;

let mut cfg = SupervisorConfig::default();
cfg.grace = Duration::from_secs(30);   // shutdown grace period
cfg.timeout = Duration::from_secs(5);  // default per-task timeout (0 = none)
cfg.max_concurrent = 4;                // task concurrency limit (0 = unlimited)
cfg.bus_capacity = 2048;               // event bus ring buffer size
cfg.restart = RestartPolicy::OnFailure;
cfg.backoff = BackoffPolicy::default();
```

| Field | Default | Meaning |
|-------|---------|---------|
| `grace` | `60s` | How long to wait for tasks to stop on shutdown |
| `timeout` | `0s` (none) | Default per-task attempt timeout |
| `max_concurrent` | `0` (unlimited) | Global semaphore for running tasks |
| `bus_capacity` | `1024` | Broadcast channel size. Slow subscribers see `Lagged` |
| `restart` | `OnFailure` | Default restart policy for tasks |
| `backoff` | `100ms / 1.0x / 30s max` | Default backoff for tasks |

---

## Controller *(feature = `controller`)*

Slot-based admission control. Tasks submit to named slots; the policy decides what happens when a slot is busy.

| Policy | Behavior |
|--------|----------|
| `Queue` | FIFO queue. New task waits until current one finishes. |
| `Replace` | Cancels running task, starts the new one immediately. |
| `DropIfRunning` | Rejects submission if slot is already busy. |

```rust
use taskvisor::{ControllerSpec, ControllerConfig};

let sup = Supervisor::builder(cfg)
    .with_controller(ControllerConfig::default())
    .build();

// Submit with admission policy
sup.submit(ControllerSpec::queue(spec)).await?;
sup.submit(ControllerSpec::replace(spec)).await?;
sup.submit(ControllerSpec::drop_if_running(spec)).await?;
```

---

## How it works

```text
TaskFn (your async code)
   |
   v
TaskSpec (task + RestartPolicy + BackoffPolicy + timeout)
   |
   v
Supervisor
   ├── Registry
   │     └── TaskActor (per task)
   │           ├── attempt loop
   │           │     ├── run task with timeout
   │           │     ├── apply RestartPolicy
   │           │     └── apply BackoffPolicy on failure
   │           └── publish events to Bus
   └── Bus
         └── Subscribers (your metrics, logs, alerts)
```

---

## Examples

```bash
cargo run --example subscriber
cargo run --example control
cargo run --example controller --features controller
```

| Example | What it shows |
|---------|--------------|
| [subscriber.rs](examples/subscriber.rs) | Custom event subscriber tracking task metrics |
| [control.rs](examples/control.rs) | Add, remove, cancel tasks at runtime |
| [controller.rs](examples/controller.rs) | Admission policies: Queue, Replace, DropIfRunning |

## Optional features

| Feature | What it enables |
|---------|----------------|
| `controller` | `Controller`, `ControllerSpec`, `ControllerConfig`, `AdmissionPolicy` |
| `logging` | Built-in `LogWriter` subscriber that prints events to stdout (demo only) |

## Contributing

Found a bug? Have an idea? Pull requests and issues are welcome.
