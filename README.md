# Taskvisor

[![Crates.io](https://img.shields.io/crates/v/taskvisor.svg)](https://crates.io/crates/taskvisor)
[![docs.rs](https://docs.rs/taskvisor/badge.svg)](https://docs.rs/taskvisor)
[![Minimum Rust 1.90](https://img.shields.io/badge/rust-1.90%2B-orange.svg)](https://rust-lang.org)
[![Apache 2.0](https://img.shields.io/badge/license-Apache2.0-blue.svg)](./LICENSE)

<div>
  <a href="https://docs.rs/taskvisor/latest/taskvisor/"><img alt="API Docs" src="https://img.shields.io/badge/API%20Docs-4d76ae?style=for-the-badge&logo=rust&logoColor=white"></a>
  <a href="./examples/"><img alt="Examples" src="https://img.shields.io/badge/Examples-2ea44f?style=for-the-badge&logo=github&logoColor=white"></a>
</div>

> Lightweight, event-driven task supervision for async Rust.

Runs your background tasks, restarts them on failure with configurable backoff, and emits structured events for every lifecycle change.

```text
TaskFn (your async code)
   ▼
TaskSpec (task + RestartPolicy + BackoffPolicy + timeout + max_retries)
   ▼
Supervisor
   ├──► Registry
   │     └──► TaskActor (per task)
   │           ├──► attempt loop
   │           │     ├──► run task with timeout + cancellation token
   │           │     ├──► apply RestartPolicy on Ok/Err
   │           │     └──► apply BackoffPolicy on failure
   │           └──► publish events to Bus
   └──► Bus (broadcast channel)
         ├──► AliveTracker (sequence-based liveness)
         └──► Subscribers (own queue each; your metrics, logs, alerts)
```

## Why taskvisor?

taskvisor gives you a **supervised** handle instead: it restarts the task on failure with backoff, narrates every step through events, and hands you the task's final outcome.

|     | Feature                   | What you get                                                                |
|:---:|---------------------------|-----------------------------------------------------------------------------|
| 🔁  | **Restart policies**      | `Never`, `OnFailure`, or `Always { interval }`: chosen per task             |
|  ⏳  | **Backoff with jitter**   | Exponential, constant, or randomized-band; spreads retries out in time      |
| 📡  | **Structured events**     | Every start, stop, failure, timeout, and backoff on a broadcast bus         |
| 🔌  | **Pluggable subscribers** | Implement one method (`on_event`) for metrics, alerts, or logging           |
| 🎯  | **Guaranteed outcomes**   | `await` a task's final `TaskOutcome`, even if events are dropped under load |
| 🎛️ | **Dynamic management**    | Add, remove, cancel tasks at runtime via `SupervisorHandle`                 |
| 🚦  | **Admission control**     | Optional slot controller: Queue / Replace / DropIfRunning                   |
| 🚧  | **Concurrency limits**    | Global semaphore, per-task timeouts, max retries                            |

**What's distinctive:** most supervision crates stop at restart + backoff:

- **Observability as a first-class plane** - a typed lifecycle event bus with pluggable subscribers, not a status field you poll.
- **Guaranteed outcomes** - `add_and_watch` / `submit_and_watch` hand you a `TaskOutcome` for every task, on a channel that survives bus lag.
- **Admission control** - slot policies (Queue / Replace / DropIfRunning) for "only one deploy at a time" or "debounce this", built in.
- **Plain async fns, no `Clone` bound** - task state lives behind `&self` and survives restarts; you don't rebuild it on every retry.

Taskvisor is not a replacement for tokio or tower. 
It works at a higher level: you write the task, and taskvisor runs it, restarts it on failure with backoff, and tells you what happened through events.

It is also not an actor framework: there are no addressable actors, mailboxes, or message passing.
Taskvisor supervises **tasks** (plain async functions), not actors.

## When to use taskvisor

Reach for it when you have **resident background tasks** that must stay up for the life of the process - queue consumers, pollers, sync loops, connection keepers, periodic jobs - and you want them restarted on failure, observable through events, and manageable (add / remove / await) at runtime.

**Not the right tool if:**

| You want…                                  | Use instead                                                                                     |
|--------------------------------------------|-------------------------------------------------------------------------------------------------|
| To retry a single future                   | [backon](https://crates.io/crates/backon) / [tokio-retry](https://crates.io/crates/tokio-retry) |
| Durable, distributed jobs with storage     | [apalis](https://crates.io/crates/apalis)                                                       |
| The actor model (addresses, mailboxes)     | [ractor](https://crates.io/crates/ractor) / [kameo](https://crates.io/crates/kameo)             |
| Structured subsystem shutdown, no restarts | [tokio-graceful-shutdown](https://crates.io/crates/tokio-graceful-shutdown)                     |

> **Roadmap:** 
> taskvisor is the supervision core of *Solti*, a larger task-orchestration toolkit in development on top of it (subprocess execution, HTTP/gRPC API, metrics, dashboards). 
> 
> Taskvisor stands on its own today: the rest is future work.

## Quick start

```toml
[dependencies]
taskvisor = "0.3"
tokio = { version = "1", features = ["full"] }
```

A task that prints "pong" every 10 seconds, restarts forever, and shuts down on Ctrl+C:

```rust,no_run
use std::time::Duration;
use taskvisor::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sup = Supervisor::builder(SupervisorConfig::default()).build();

    let ping: TaskRef = TaskFn::arc("ping", |ctx: TaskContext| async move {
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

## See it recover from failure

A task that fails twice, then succeeds - taskvisor retries it with backoff and publishes an event for every step. 
A subscriber prints them, and you *see* the supervision happen:

```rust,no_run
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use taskvisor::prelude::*;

/// A subscriber that prints every lifecycle event the supervisor emits for a task.
struct Printer;
impl Subscribe for Printer {
    fn on_event(&self, ev: &Event) {
        if let Some(task) = ev.task.as_deref() {
            println!("  {:?} (task={task})", ev.kind);
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let attempts = Arc::new(AtomicU32::new(0));
    let flaky: TaskRef = TaskFn::arc("flaky", move |_ctx: TaskContext| {
        let attempts = Arc::clone(&attempts);
        async move {
            let n = attempts.fetch_add(1, Ordering::Relaxed) + 1;
            if n < 3 {
                Err(TaskError::fail(format!("boom #{n}")))
            } else {
                Ok(()) // 3rd attempt succeeds
            }
        }
    });

    // Restart on failure, with a short backoff between attempts.
    let backoff = BackoffPolicy::new(
        Duration::from_millis(50),
        Duration::from_secs(30),
        1.0,
        JitterPolicy::None,
    )
    .unwrap();
    let spec = TaskSpec::restartable(flaky).with_backoff(backoff);

    let subs: Vec<Arc<dyn Subscribe>> = vec![Arc::new(Printer)];
    Supervisor::new(SupervisorConfig::default(), subs).run(vec![spec]).await?;
    Ok(())
}
```

```text
# output:
  TaskAddRequested (task=flaky)
  TaskAdded (task=flaky)
  TaskStarting (task=flaky)        # attempt 1
  TaskFailed (task=flaky)
  BackoffScheduled (task=flaky)    # wait, then retry
  TaskStarting (task=flaky)        # attempt 2
  TaskFailed (task=flaky)
  BackoffScheduled (task=flaky)
  TaskStarting (task=flaky)        # attempt 3
  TaskStopped (task=flaky)         # success
  ActorExhausted (task=flaky)      # OnFailure + success = done
  TaskRemoved (task=flaky)
```

Restart, backoff, and an event for every step, without writing a retry loop.

See [`examples/metrics.rs`](examples/metrics.rs) for a fuller version (`cargo run --example metrics`).


## Two modes

| Mode             | When to use            | Lifecycle                                        |
|------------------|------------------------|--------------------------------------------------|
| `sup.run(specs)` | Tasks known upfront    | Blocks until all done or Ctrl+C                  |
| `sup.serve()`    | Tasks added at runtime | Returns `SupervisorHandle`, you control shutdown |

```rust,ignore
// Dynamic mode
let handle = sup.serve();

let id = handle.add(spec)?;                       // returns a TaskId
handle.cancel(id).await?;                         // cancel by identity
handle.remove(id)?;                               // or remove by identity
handle.cancel_by_label("task-name").await?;       // ...or by label
let alive = handle.is_alive("task-name").await;
let tasks = handle.list().await;                  // Vec<(TaskId, name)>

handle.shutdown().await?;
```

## Core concepts

**Task & TaskFn** - A `Task` is any `Send + Sync + 'static` type that implements
```rust,ignore
fn spawn(&self, ctx: TaskContext) -> BoxTaskFuture
``` 
`TaskFn` wraps a closure into a `Task` so you don't need a struct. `TaskRef` is just `Arc<dyn Task>`.

**TaskSpec** - Bundles a task with its policies: restart, backoff, timeout, and max retries. 
This is what you pass to the supervisor.

```rust,ignore
// One-shot (run once, never restart)
let spec = TaskSpec::once(task);

// Restartable (restart on failure, stop on success)
let spec = TaskSpec::restartable(task);

// Full control
let spec = TaskSpec::new(task, RestartPolicy::Always { interval: None }, backoff, Some(timeout))
    .with_max_retries(std::num::NonZeroU32::new(5).unwrap()); // None (omit) = unlimited
```

**RestartPolicy** - Controls when a task restarts after it exits:

| Policy                  | Behavior                                        |
|-------------------------|-------------------------------------------------|
| `Never`                 | Runs once, then stops.                          |
| `OnFailure` *(default)* | Restarts only after an error; stops on success. |
| `Always { interval }`   | Always restarts.                                |

**BackoffPolicy** - Controls retry delay after failure. 
Delay for attempt `n` = `first * factor^n`, capped at `max`, then jitter is applied.
A non-zero delay is floored to at least `1ms` (capped at `max`) to avoid a restart hot-spin (`first = 0` opts out):

| Field    | Default  | Meaning                                      |
|----------|----------|----------------------------------------------|
| `first`  | `100ms`  | Initial delay                                |
| `max`    | `30s`    | Delay cap                                    |
| `factor` | `1.0`    | Multiplier per attempt (`2.0` = exponential) |
| `jitter` | `None`   | Randomization strategy (see below)           |

**JitterPolicy** - adds jitter to retry delays to prevent many tasks from retrying at the same time:

| Policy                  | Range                            | Use when                               |
|-------------------------|----------------------------------|----------------------------------------|
| `None`                  | exact delay                      | Single task, predictable timing        |
| `Full`                  | `[0, delay]`                     | Maximum spread needed                  |
| `Equal` *(recommended)* | `[delay/2, delay]`               | Balanced; keeps about 75% of the delay |
| `RandomizedBand`        | `[first, min(base*3, max)]`      | Widest spread; can exceed the base     |

**Events & Subscribe** - Every lifecycle change is published to a broadcast bus. 
Implement `Subscribe` to observe them. 
Each subscriber gets its own bounded queue - a slow subscriber never blocks others or the supervisor.

## Error handling

Return these from your task to control what happens next:

| Return                           | Retryable | What happens                                                                |
|----------------------------------|-----------|-----------------------------------------------------------------------------|
| `Ok(())`                         | -         | Task completed. `RestartPolicy` decides next step.                          |
| `panic!` in the task body        | Yes\*     | Caught and converted to `TaskError::Fail`; backoff, then retry per policy.  |
| `Err(TaskError::fail(reason))`   | Yes       | Retryable failure. Backoff, then retry. Wrap a cause with `fail_from(err)`. |
| `Err(TaskError::Timeout { .. })` | Yes       | Set automatically when per-task timeout is exceeded.                        |
| `Err(TaskError::fatal(reason))`  | No        | Permanent failure. Actor stops, publishes `ActorDead`. See `fatal_from`.    |
| `Err(TaskError::Canceled)`       | No        | Graceful shutdown. Not an error.                                            |

`exit_code` is `Option<i32>`: use when the error comes from a process-like runtime, pass `None` for logical errors. 
Subscribers receive it as `Event::exit_code` on `TaskFailed` / `ActorDead` / `ActorExhausted`.

### Cancellation

Tasks **must** observe cancellation via the `TaskContext` passed to `spawn`:

```rust,ignore
// Pattern 1: select! (recommended for long-running tasks)
tokio::select! {
    _ = ctx.cancelled() => Err(TaskError::Canceled),
    result = do_work() => result,
}

// Pattern 2: check before work (ok for short tasks)
if ctx.is_cancelled() { return Err(TaskError::Canceled); }
do_work().await
```

## Recipes

### Exponential backoff with jitter

```rust,no_run
use std::time::Duration;
use taskvisor::{BackoffPolicy, JitterPolicy};

let backoff = BackoffPolicy::new(
    Duration::from_millis(200),     // first
    Duration::from_secs(30),        // max
    2.0,                            // factor: 200ms -> 400ms -> 800ms -> ...
    JitterPolicy::Equal,            // recommended: [delay/2, delay]
)
.unwrap();
// optional: .with_floor(Duration::from_millis(50)) to keep jittered delays above a minimum
```

### Per-task timeout with max retries

```rust,ignore
// Task gets 5s per attempt, max 3 retries.
// If exceeded: TimeoutHit event + TaskError::Timeout + backoff + retry.
let spec = TaskSpec::new(task, RestartPolicy::OnFailure, backoff, Some(Duration::from_secs(5)))
    .with_max_retries(std::num::NonZeroU32::new(3).unwrap());
```

### Custom subscriber (metrics)

```rust,no_run
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use taskvisor::{Subscribe, Event, EventKind, Supervisor, SupervisorConfig};

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
    .with_subscribers(vec![metrics as Arc<dyn Subscribe>])
    .build();
```

### Periodic task (run every N seconds)

```rust,ignore
// Runs, completes, waits 30s, runs again. Forever.
let spec = TaskSpec::restartable(task)
    .with_restart(RestartPolicy::Always { interval: Some(Duration::from_secs(30)) });
```

### Await a task's final result

```rust,ignore
// add_and_watch returns a TaskWaiter resolving to the final TaskOutcome.
// Delivery is guaranteed (oneshot, not subject to event-bus lag).
let (id, waiter) = handle
    .add_and_watch(TaskSpec::once(job), Duration::from_secs(1))
    .await?;

match waiter.wait().await? {
    TaskOutcome::Completed => println!("{id} done"),
    TaskOutcome::Failed { reason, .. } => eprintln!("{id} failed: {reason}"),
    other => eprintln!("{id} ended with {other:?}"),
}
```

## Events

Every lifecycle change publishes an `Event` to the bus. Subscribe to observe them.

| Event                                      | Meaning                                              |
|--------------------------------------------|------------------------------------------------------|
| **Lifecycle**                              |                                                      |
| `TaskStarting`                             | Attempt is beginning                                 |
| `TaskStopped`                              | Attempt completed successfully                       |
| `TaskCanceled`                             | Attempt exited via graceful cancellation             |
| `TaskFailed`                               | Attempt failed (retryable or fatal)                  |
| `TimeoutHit`                               | Attempt exceeded its timeout                         |
| `BackoffScheduled`                         | Retry delay scheduled (includes delay duration)      |
| **Terminal**                               |                                                      |
| `ActorExhausted`                           | Restart policy says stop; the normal way a task ends |
| `ActorDead`                                | Fatal error, actor will not restart                  |
| **Management**                             |                                                      |
| `TaskAdded` / `TaskRemoved`                | Task registered / unregistered                       |
| `TaskAddFailed`                            | Add rejected: a task with that name already exists   |
| `TaskAddRequested` / `TaskRemoveRequested` | Add/remove commands received                         |
| **Shutdown**                               |                                                      |
| `ShutdownRequested`                        | OS signal caught (SIGTERM/SIGINT)                    |
| `AllStoppedWithinGrace`                    | Clean shutdown                                       |
| `GraceExceeded`                            | Some tasks didn't stop in time                       |
| **Subscriber**                             |                                                      |
| `SubscriberPanicked`                       | A subscriber panicked (isolated, others unaffected)  |
| `SubscriberOverflow`                       | Subscriber queue full, event dropped                 |
| **Controller** *(feature = `controller`)*  |                                                      |
| `ControllerSubmitted`                      | Task accepted into a slot                            |
| `ControllerRejected`                       | Submission rejected (slot busy, queue full, ...)     |
| `ControllerSlotTransition`                 | Slot changed state (e.g. running → terminating)      |

Each event carries: `kind`, `id` (the task's `TaskId`), `task` (name), `attempt`, `reason`, `exit_code`, `delay_ms`, `timeout_ms`, `backoff_source`, `seq` (monotonic ordering), `at` (timestamp).

## Configuration

```rust,no_run
use std::num::{NonZeroU32, NonZeroUsize};
use std::time::Duration;
use taskvisor::{SupervisorConfig, RestartPolicy, BackoffPolicy};

let mut cfg = SupervisorConfig::default();
cfg.grace = Duration::from_secs(30);              // shutdown grace period
cfg.timeout = Some(Duration::from_secs(5));       // default per-task timeout (None = no timeout)
cfg.max_retries = NonZeroU32::new(10);            // default max retries (None = unlimited)
cfg.max_concurrent = NonZeroUsize::new(4);        // task concurrency limit (None = unlimited)
cfg.bus_capacity = 2048;                          // event bus ring buffer size
cfg.restart = RestartPolicy::OnFailure;
cfg.backoff = BackoffPolicy::default();
```

| Field            | Default                  | Meaning                                                |
|------------------|--------------------------|--------------------------------------------------------|
| `grace`          | `60s`                    | How long to wait for tasks to stop on shutdown         |
| `timeout`        | `None`                   | Default per-task attempt timeout (`None` = no timeout) |
| `max_retries`    | `None` (unlimited)       | Default max failure-driven retries (`Some(n)` = limit) |
| `max_concurrent` | `None` (unlimited)       | Global semaphore for running tasks (`Some(n)` permits) |
| `bus_capacity`   | `1024`                   | Broadcast channel size. Slow subscribers see `Lagged`  |
| `restart`        | `OnFailure`              | Default restart policy for tasks                       |
| `backoff`        | `100ms / 1.0x / 30s max` | Default backoff for tasks                              |

## Controller *(feature = `controller`)*

Slot-based admission control. 
Tasks submit to named slots, the policy decides what happens when a slot is busy.

| Policy          | Behavior                                               |
|-----------------|--------------------------------------------------------|
| `Queue`         | FIFO queue. New task waits until current one finishes. |
| `Replace`       | Cancels running task, starts the new one immediately.  |
| `DropIfRunning` | Rejects submission if slot is already busy.            |

```rust,ignore
use taskvisor::{ControllerSpec, ControllerConfig};

let sup = Supervisor::builder(cfg)
    .with_controller(ControllerConfig::default())
    .build();

let handle = sup.serve();

// submit() returns a pre-allocated TaskId immediately; 
// the final admission result is reported asynchronously via ControllerSubmitted or ControllerRejected, both carrying the same TaskId.
let id = handle.submit(ControllerSpec::queue(spec)).await?;
handle.submit(ControllerSpec::replace(spec)).await?;
handle.submit(ControllerSpec::drop_if_running(spec)).await?;

// ...or await the outcome directly. If the slot never admits it (busy, queue full, superseded, removed, shutting down) the waiter resolves to TaskOutcome::Rejected.
let (id, waiter) = handle.submit_and_watch(ControllerSpec::queue(spec)).await?;
let outcome = waiter.wait().await?;

// Inspect the controller's live state directly - no parsing of bus events.
// `controller_snapshot()` is the slot-path analogue of `list()`/`snapshot()`.
if let Some(snap) = handle.controller_snapshot().await {
    println!("{} running, {} queued", snap.running_count(), snap.total_queued());
    let depth = snap.slot("deploy").map_or(0, |s| s.queue_depth);
}

handle.shutdown().await?;
```

## Performance

Per-task overhead is a few microseconds: a handful of events on the bus, plus one registry insert and remove. 
That is small next to real task work, where I/O takes milliseconds or more. 
The cost grows linearly with the number of tasks, subscribers, and batch size.

Run the benchmarks on your own hardware:

```bash
cargo bench                                          # all benchmarks
cargo bench --bench lifecycle                        # specific suite
cargo bench --bench controller --features controller # controller benchmarks
```

## Examples

```bash
cargo run --example basic
cargo run --example worker
cargo run --example periodic
cargo run --example multiple
cargo run --example metrics
cargo run --example dynamic
cargo run --example outcomes
cargo run --example pipeline --features controller
cargo run --example admission --features controller
```

| Example                                   | What it shows                                                  |
|-------------------------------------------|----------------------------------------------------------------|
| [basic.rs](examples/basic.rs)             | Minimal hello-world, one task runs once                        |
| [worker.rs](examples/worker.rs)           | Long-running worker with graceful Ctrl+C shutdown              |
| [periodic.rs](examples/periodic.rs)       | Cron-like periodic task via `RestartPolicy::Always`            |
| [multiple.rs](examples/multiple.rs)       | Three tasks with different policies and backoff                |
| [metrics.rs](examples/metrics.rs)         | Custom `Subscribe` implementation for metrics                  |
| [dynamic.rs](examples/dynamic.rs)         | `serve()` + `SupervisorHandle`: add/remove/cancel at runtime   |
| [outcomes.rs](examples/outcomes.rs)       | `add_and_watch`: await a task's final `TaskOutcome`            |
| [pipeline.rs](examples/pipeline.rs)       | Controller admission policies: Queue, Replace, DropIfRunning   |
| [admission.rs](examples/admission.rs)     | `submit_and_watch`: await admission outcome (incl. `Rejected`) |

## Optional features

| Feature      | What it enables                                                                       |
|--------------|---------------------------------------------------------------------------------------|
| `controller` | Slot-based admission control: `ControllerSpec`, `ControllerConfig`, `AdmissionPolicy` |
| `logging`    | Built-in `LogWriter` subscriber - event output to stdout (demo/reference)             |

```toml
taskvisor = { version = "0.3", features = ["controller", "logging"] }
```

## Contributing

Found a bug? Have an idea? [Open an issue](https://github.com/soltiHQ/taskvisor/issues) or send a pull request.
