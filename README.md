# Taskvisor

> Lightweight, event-driven task supervision for async Rust.

Inspired by Erlang/OTP supervisors. Runs your background tasks, restarts them on failure with configurable backoff, and emits structured events for every lifecycle change.

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

[![Crates.io](https://img.shields.io/crates/v/taskvisor.svg)](https://crates.io/crates/taskvisor)
[![docs.rs](https://docs.rs/taskvisor/badge.svg)](https://docs.rs/taskvisor)
[![Minimum Rust 1.85](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://rust-lang.org)
[![Apache 2.0](https://img.shields.io/badge/license-Apache2.0-blue.svg)](./LICENSE)

## Why taskvisor?

Tokio gives you `spawn` and `JoinHandle`, but no supervision, no restart policies, no backoff, and no observability.
If a spawned task panics or fails, you find out when the `JoinHandle` is polled - or never.

Taskvisor fills that gap:
- **Restart policies** - `Never`, `OnFailure`, `Always { interval }` per task
- **Backoff with jitter** - exponential, constant, or decorrelated; prevents thundering herd
- **Structured events** - every start, stop, failure, timeout, and backoff is published to a broadcast bus
- **Pluggable subscribers** - implement one method (`on_event`) to hook in metrics, alerting, or logging
- **Dynamic management** - add, remove, cancel tasks at runtime via `SupervisorHandle`
- **Admission control** - optional slot-based controller with Queue / Replace / DropIfRunning policies
- **Concurrency limits** - global semaphore, per-task timeouts, max retries
- **Zero unsafe** - pure safe Rust

> Need a complete task-orchestration **agent**: subprocess execution, HTTP/gRPC API, Prometheus metrics, control-plane discovery, ready Grafana dashboards? 
> 
> See [Solti SDK](https://github.com/soltiHQ/sdk): built on top of taskvisor.

---

## Quick start

```toml
[dependencies]
taskvisor = "0.1.1"
tokio = { version = "1", features = ["full"] }
```

A task that prints "pong" every 10 seconds, restarts forever, and shuts down on Ctrl+C:

```rust
use std::time::Duration;
use taskvisor::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sup = Supervisor::builder(SupervisorConfig::default()).build();

    let ping: TaskRef = TaskFn::arc("ping", |ctx: CancellationToken| async move {
        tokio::select! {
            _ = ctx.cancelled() => return Err(TaskError::Canceled),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        println!("[ping] pong");
        Ok(())
    });

    let spec = TaskSpec::restartable(ping)
        .with_restart(RestartPolicy::Always { interval: Some(Duration::from_secs(10)) });

    sup.run(vec![spec]).await?;
    Ok(())
}
```

---

## Two modes

| Mode             | When to use            | Lifecycle                                        |
|------------------|------------------------|--------------------------------------------------|
| `sup.run(specs)` | Tasks known upfront    | Blocks until all done or Ctrl+C                  |
| `sup.serve()`    | Tasks added at runtime | Returns `SupervisorHandle`, you control shutdown |

```rust
// Dynamic mode
let handle = sup.serve();

handle.add(spec)?;
handle.cancel("task-name").await?;
handle.remove("task-name")?;
let alive = handle.is_alive("task-name").await;
let tasks = handle.list().await;

handle.shutdown().await?;
```

---

## Core concepts

**Task & TaskFn** - A `Task` is any `Send + Sync + 'static` type that implements `fn spawn(&self, ctx: CancellationToken) -> BoxTaskFuture`. 
`TaskFn` wraps a closure into a `Task` so you don't need a struct. `TaskRef` is just `Arc<dyn Task>`.

**TaskSpec** - Bundles a task with its policies: restart, backoff, timeout, and max retries. This is what you pass to the supervisor.

```rust
// One-shot (run once, never restart)
let spec = TaskSpec::once(task);

// Restartable (restart on failure, stop on success)
let spec = TaskSpec::restartable(task);

// Full control
let spec = TaskSpec::new(task, RestartPolicy::Always { interval: None }, backoff, Some(timeout))
    .with_max_retries(5);
```

**RestartPolicy** - Controls when a task restarts after it exits:

| Policy                  | Behavior                                                       |
|-------------------------|----------------------------------------------------------------|
| `Never`                 | Run once, done.                                                |
| `OnFailure` *(default)* | Restart only on error. Success = stop.                         |
| `Always { interval }`   | Always restart. `interval: Some(10s)` waits between successes. |

**BackoffPolicy** - Controls retry delay after failure. Delay for attempt `n` = `first * factor^n`, capped at `max`, then jitter is applied:

| Field    | Default  | Meaning                                      |
|----------|----------|----------------------------------------------|
| `first`  | `100ms`  | Initial delay                                |
| `max`    | `30s`    | Delay cap                                    |
| `factor` | `1.0`    | Multiplier per attempt (`2.0` = exponential) |
| `jitter` | `None`   | Randomization strategy (see below)           |

**JitterPolicy** - Prevents thundering herd when multiple tasks retry at the same time:

| Policy                  | Range                            | Use when                            |
|-------------------------|----------------------------------|-------------------------------------|
| `None`                  | exact delay                      | Single task, predictable timing     |
| `Full`                  | `[0, delay]`                     | Maximum spread needed               |
| `Equal` *(recommended)* | `[delay/2, delay]`               | Balanced: preserves ~75% of backoff |
| `Decorrelated`          | `[base, base*3]` capped at `max` | Sophisticated, self-adjusting       |

**Events & Subscribe** - Every lifecycle change is published to a broadcast bus. 
Implement `Subscribe` to observe them. 
Each subscriber gets its own bounded queue - a slow subscriber never blocks others or the supervisor.

---

## Error handling

Return these from your task to control what happens next:

| Return                                         | Retryable | What happens                                           |
|------------------------------------------------|-----------|--------------------------------------------------------|
| `Ok(())`                                       | -         | Task completed. `RestartPolicy` decides next step.     |
| `Err(TaskError::Fail { reason, exit_code })`   | Yes       | Retryable failure. Backoff, then retry.                |
| `Err(TaskError::Timeout { .. })`               | Yes       | Set automatically when per-task timeout is exceeded.   |
| `Err(TaskError::Fatal { reason, exit_code })`  | No        | Permanent failure. Actor stops, publishes `ActorDead`. |
| `Err(TaskError::Canceled)`                     | No        | Graceful shutdown. Not an error.                       |

`exit_code` is `Option<i32>`: use when the error comes from a process-like runtime (subprocess, WASI), pass `None` for logical errors. 
Subscribers receive it as `Event::exit_code` on `TaskFailed` / `ActorDead` / `ActorExhausted`.

### Cancellation

Tasks **must** observe cancellation via the `CancellationToken` passed to `spawn`:

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

### Per-task timeout with max retries

```rust
// Task gets 5s per attempt, max 3 retries.
// If exceeded: TimeoutHit event + TaskError::Timeout + backoff + retry.
let spec = TaskSpec::new(task, RestartPolicy::OnFailure, backoff, Some(Duration::from_secs(5)))
    .with_max_retries(3);
```

### Custom subscriber (metrics)

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use taskvisor::{Subscribe, Event, EventKind};

struct Metrics { failures: AtomicU64 }

impl Subscribe for Metrics {
    fn on_event(&self, event: &Event) {
        if matches!(event.kind, EventKind::TaskFailed) {
            self.failures.fetch_add(1, Ordering::Relaxed);
        }
    }
}

let metrics = Arc::new(Metrics { failures: AtomicU64::new(0) });
let sup = Supervisor::builder(SupervisorConfig::default())
    .with_subscribers(vec![metrics])
    .build();
```

### Periodic task (run every N seconds)

```rust
// Runs, completes, waits 30s, runs again. Forever.
let spec = TaskSpec::restartable(task)
    .with_restart(RestartPolicy::Always { interval: Some(Duration::from_secs(30)) });
```

---

## Events

Every lifecycle change publishes an `Event` to the bus. Subscribe to observe them.

| Event                                      | Meaning                                             |
|--------------------------------------------|-----------------------------------------------------|
| **Lifecycle**                              |                                                     |
| `TaskStarting`                             | Attempt is beginning                                |
| `TaskStopped`                              | Completed or cancelled gracefully                   |
| `TaskFailed`                               | Attempt failed (retryable or fatal)                 |
| `TimeoutHit`                               | Attempt exceeded its timeout                        |
| `BackoffScheduled`                         | Retry delay scheduled (includes delay duration)     |
| **Terminal**                               |                                                     |
| `ActorExhausted`                           | Restart policy says stop (normal end-of-life)       |
| `ActorDead`                                | Fatal error, actor will not restart                 |
| **Management**                             |                                                     |
| `TaskAdded` / `TaskRemoved`                | Task registered / unregistered                      |
| `TaskAddRequested` / `TaskRemoveRequested` | Add/remove commands received                        |
| **Shutdown**                               |                                                     |
| `ShutdownRequested`                        | OS signal caught (SIGTERM/SIGINT)                   |
| `AllStoppedWithinGrace`                    | Clean shutdown                                      |
| `GraceExceeded`                            | Some tasks didn't stop in time                      |
| **Subscriber**                             |                                                     |
| `SubscriberPanicked`                       | A subscriber panicked (isolated, others unaffected) |
| `SubscriberOverflow`                       | Subscriber queue full, event dropped                |

Each event carries: `kind`, `task` (name), `attempt`, `reason`, `delay_ms`, `timeout_ms`, `seq` (monotonic ordering), `at` (timestamp).

---

## Configuration

```rust
use std::time::Duration;
use taskvisor::{SupervisorConfig, RestartPolicy, BackoffPolicy};

let mut cfg = SupervisorConfig::default();
cfg.grace = Duration::from_secs(30);        // shutdown grace period
cfg.timeout = Duration::from_secs(5);       // default per-task timeout (0 = none)
cfg.max_retries = 10;                       // default max retries (0 = unlimited)
cfg.max_concurrent = 4;                     // task concurrency limit (0 = unlimited)
cfg.bus_capacity = 2048;                    // event bus ring buffer size
cfg.restart = RestartPolicy::OnFailure;
cfg.backoff = BackoffPolicy::default();
```

| Field            | Default                  | Meaning                                               |
|------------------|--------------------------|-------------------------------------------------------|
| `grace`          | `60s`                    | How long to wait for tasks to stop on shutdown        |
| `timeout`        | `0s` (none)              | Default per-task attempt timeout                      |
| `max_retries`    | `0` (unlimited)          | Default max failure-driven retries                    |
| `max_concurrent` | `0` (unlimited)          | Global semaphore for running tasks                    |
| `bus_capacity`   | `1024`                   | Broadcast channel size. Slow subscribers see `Lagged` |
| `restart`        | `OnFailure`              | Default restart policy for tasks                      |
| `backoff`        | `100ms / 1.0x / 30s max` | Default backoff for tasks                             |

---

## Controller *(feature = `controller`)*

Slot-based admission control. Tasks submit to named slots; the policy decides what happens when a slot is busy.

| Policy          | Behavior                                               |
|-----------------|--------------------------------------------------------|
| `Queue`         | FIFO queue. New task waits until current one finishes. |
| `Replace`       | Cancels running task, starts the new one immediately.  |
| `DropIfRunning` | Rejects submission if slot is already busy.            |

```rust
use taskvisor::{ControllerSpec, ControllerConfig};

let sup = Supervisor::builder(cfg)
    .with_controller(ControllerConfig::default())
    .build();

let handle = sup.serve();

handle.submit(ControllerSpec::queue(spec)).await?;
handle.submit(ControllerSpec::replace(spec)).await?;
handle.submit(ControllerSpec::drop_if_running(spec)).await?;

handle.shutdown().await?;
```

---

## How it works

```text
TaskFn (your async code)
   |
   v
TaskSpec (task + RestartPolicy + BackoffPolicy + timeout + max_retries)
   |
   v
Supervisor
   ├── Registry
   │     └── TaskActor (per task)
   │           ├── attempt loop
   │           │     ├── run task with timeout + cancellation token
   │           │     ├── apply RestartPolicy on Ok/Err
   │           │     └── apply BackoffPolicy on failure
   │           └── publish events to Bus
   └── Bus (broadcast channel)
         ├── AliveTracker (sequence-based liveness)
         └── Subscribers (your metrics, logs, alerts)
```

---

## Performance

Measured with [Criterion](https://github.com/bheisler/criterion.rs) on hardware ranging from Raspberry Pi 4 to Apple M3 Max. 
Numbers below are order-of-magnitude estimates: run `cargo bench` on your machine for precise results.

| What                      | Order of magnitude  | What it tells you                                                |
|---------------------------|---------------------|------------------------------------------------------------------|
| Per-task overhead         | ~10-50 us           | Full lifecycle (spawn, run, cleanup) for one no-op task          |
| `handle.add()` latency    | ~100-800 ns         | Submitting a task via `serve()` API. Channel send, no I/O        |
| Batch throughput          | ~50K-400K tasks/sec | Processing N instant tasks via `run()`. Scales with CPU          |
| Fan-out (0-8 subscribers) | ~2x total time      | Each subscriber gets its own queue. Linear growth, no contention |
| `add_and_wait` + `cancel` | ~30-200 us          | Full round-trip: register, confirm, cancel                       |
| `list()` with 500 tasks   | ~5-30 us            | Registry snapshot via async channel                              |

Throughput scales linearly with batch size, subscriber overhead is linear per subscriber, and `list()` is linear in the number of registered tasks. 
No known superlinear bottlenecks.

```bash
cargo bench                                          # all benchmarks
cargo bench --bench lifecycle                        # specific suite
cargo bench --bench controller --features controller # controller benchmarks
```

---

## Examples

```bash
cargo run --example basic
cargo run --example worker
cargo run --example periodic
cargo run --example multiple_tasks
cargo run --example metrics
cargo run --example dynamic
cargo run --example pipeline --features controller
```

| Example                                   | What it shows                                                |
|-------------------------------------------|--------------------------------------------------------------|
| [basic.rs](examples/basic.rs)             | Minimal hello-world, one task runs once                      |
| [worker.rs](examples/worker.rs)           | Long-running worker with graceful Ctrl+C shutdown            |
| [periodic.rs](examples/periodic.rs)       | Cron-like periodic task via `RestartPolicy::Always`          |
| [multiple_tasks.rs](examples/multiple.rs) | Three tasks with different policies and backoff              |
| [metrics.rs](examples/metrics.rs)         | Custom `Subscribe` implementation for metrics                |
| [dynamic.rs](examples/dynamic.rs)         | `serve()` + `SupervisorHandle`: add/remove/cancel at runtime |
| [pipeline.rs](examples/pipeline.rs)       | Controller admission policies: Queue, Replace, DropIfRunning |

---

## Optional features

| Feature      | What it enables                                                                       |
|--------------|---------------------------------------------------------------------------------------|
| `controller` | Slot-based admission control: `ControllerSpec`, `ControllerConfig`, `AdmissionPolicy` |
| `logging`    | Built-in `LogWriter` subscriber - structured event output via `tracing`               |

```toml
taskvisor = { version = "0.1", features = ["controller", "logging"] }
```

---

## Comparison with alternatives

|                         | taskvisor                         | bare tokio::spawn | tower          | backon |
|-------------------------|-----------------------------------|-------------------|----------------|--------|
| Restart policies        | Per-task (Never/OnFailure/Always) | Manual            | No             | No     |
| Backoff with jitter     | Built-in (4 strategies)           | Manual            | Via middleware | Yes    |
| Lifecycle events        | Full bus with subscribers         | JoinHandle only   | No             | No     |
| Dynamic task management | add/remove/cancel at runtime      | Manual            | No             | No     |
| Admission control       | Queue/Replace/DropIfRunning       | No                | No             | No     |
| Concurrency limits      | Global semaphore                  | Manual            | Via middleware | No     |

Taskvisor `is not` a replacement for tokio or tower.  
It sits one level above: you write the task, taskvisor runs it, restarts it, and tells you what happened.

---

## Contributing

Found a bug? Have an idea? [Open an issue](https://github.com/soltiHQ/taskvisor/issues) or send a pull request.

<div>
  <a href="https://docs.rs/taskvisor/latest/taskvisor/"><img alt="API Docs" src="https://img.shields.io/badge/API%20Docs-4d76ae?style=for-the-badge&logo=rust&logoColor=white"></a>
  <a href="./examples/"><img alt="Examples" src="https://img.shields.io/badge/Examples-2ea44f?style=for-the-badge&logo=github&logoColor=white"></a>
  <a href="https://github.com/soltiHQ/sdk"><img alt="Solti SDK" src="https://img.shields.io/badge/Solti%20SDK-cc6633?style=for-the-badge&logo=rust&logoColor=white"></a>
</div>
