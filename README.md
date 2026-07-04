# Taskvisor

[![Crates.io](https://img.shields.io/crates/v/taskvisor.svg)](https://crates.io/crates/taskvisor)
[![docs.rs](https://docs.rs/taskvisor/badge.svg)](https://docs.rs/taskvisor)
[![Minimum Rust 1.90](https://img.shields.io/badge/rust-1.90%2B-orange.svg)](https://rust-lang.org)
[![Apache 2.0](https://img.shields.io/badge/license-Apache2.0-blue.svg)](./LICENSE)

> Task supervisor for Tokio with backoff, graceful shutdown, and lifecycle events.

You write the task as a plain async fn. Taskvisor keeps it alive: it restarts tasks on failure with backoff, stops everything cleanly on shutdown, and reports every step through typed events.

## The loop you stop writing

Every long-running service has this code somewhere:

```rust,ignore
// The hand-rolled way: tokio::spawn + retry loop.
tokio::spawn(async move {
    loop {
        match run_worker().await {
            Ok(()) => break,
            Err(e) => {
                eprintln!("worker failed: {e}; retrying in 1s");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
});
// No backoff. No jitter. No graceful shutdown. No visibility.
```

With taskvisor, the same worker needs one line:

```rust,ignore
sup.run(vec![TaskSpec::restartable(worker)]).await?;
// Restart on failure, backoff, Ctrl+C handling: included.
```

## Quick start

```toml
[dependencies]
taskvisor = "0.4"
tokio = { version = "1", features = ["full"] }
```

A worker that polls every 5 seconds and stops cleanly on Ctrl+C. If it returns an error, taskvisor restarts it:

```rust,no_run
use std::time::Duration;
use taskvisor::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let worker = TaskFn::arc("worker", |ctx| async move {
        loop {
            // Resolves to Err(TaskError::Canceled) on shutdown.
            // Canceled is a clean stop, not a failure.
            ctx.run_until_cancelled(tokio::time::sleep(Duration::from_secs(5)))
                .await?;
            println!("[worker] polling...");
        }
    });

    let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
    sup.run(vec![TaskSpec::restartable(worker)]).await?;
    Ok(())
}
```

No signal handling, no retry loop, no type annotations on the closure.

## See it recover from failure

A task fails twice, then succeeds. Taskvisor retries it with backoff and publishes an event for every step. A small subscriber prints each event:

```rust,no_run
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use taskvisor::prelude::*;

/// Prints every lifecycle event, using stable machine-readable labels.
struct Printer;
impl Subscribe for Printer {
    fn on_event(&self, ev: &Event) {
        if let Some(task) = ev.task.as_deref() {
            println!("  {} (task={task})", ev.kind.as_label());
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let attempts = Arc::new(AtomicU32::new(0));
    let flaky = TaskFn::arc("flaky", move |_ctx| {
        let attempts = Arc::clone(&attempts);
        async move {
            let n = attempts.fetch_add(1, Ordering::Relaxed) + 1;
            if n < 3 {
                return Err(TaskError::fail(format!("boom #{n}")));
            }
            Ok(()) // third attempt succeeds
        }
    });

    let spec = TaskSpec::restartable(flaky)
        .with_backoff(BackoffPolicy::constant(Duration::from_millis(50)));

    let subs: Vec<Arc<dyn Subscribe>> = vec![Arc::new(Printer)];
    Supervisor::new(SupervisorConfig::default(), subs)
        .run(vec![spec])
        .await?;
    Ok(())
}
```

```text
# output:
  task_add_requested (task=flaky)
  task_added (task=flaky)
  task_starting (task=flaky)        # attempt 1
  task_failed (task=flaky)
  backoff_scheduled (task=flaky)    # wait 50ms, then retry
  task_starting (task=flaky)        # attempt 2
  task_failed (task=flaky)
  backoff_scheduled (task=flaky)
  task_starting (task=flaky)        # attempt 3
  task_stopped (task=flaky)         # success
  actor_exhausted (task=flaky)      # task succeeded, no more restarts
  task_removed (task=flaky)
```

In the output, `actor` means taskvisor's internal per-task runner, not an actor-model actor.

## Why taskvisor?

Restart and backoff are the baseline. Taskvisor also gives you:

- **A final answer per task.** Await one result: done, failed, or canceled. Delivered even when events are dropped under load.
- **Typed lifecycle events.** Every start, failure, and retry goes to an event bus. A `tracing` bridge is built in.
- **Admission control.** Tasks can share a named slot. A rule decides what happens when the slot is busy: queue, replace, or drop the new task.
- **Restart policies per task.** Never, on failure, always, or periodic.
- **Backoff with jitter.** Exponential or constant, with a cap and a floor.
- **Limits.** Per-attempt timeout, retry budget, global concurrency cap.

Taskvisor is not a replacement for Tokio or Tower.
It works one level higher: you write the task, taskvisor runs it and tells you what happened.

It is also not an actor framework: no addressable actors, no mailboxes, no message passing.
Your existing `async fn` is the unit of supervision.

## When to use taskvisor

Use it when you have **resident background tasks** that must stay up for the life of the process: queue consumers, pollers, sync loops, connection keepers, periodic jobs.
Taskvisor restarts them on failure and reports every step as an event. You can add, remove, and await tasks at runtime.

**Not the right tool if:**

| You want…                                  | Use instead                                                                                     |
|--------------------------------------------|-------------------------------------------------------------------------------------------------|
| To retry a single future                   | [backon](https://crates.io/crates/backon) / [tokio-retry](https://crates.io/crates/tokio-retry) |
| Durable, distributed jobs with storage     | [apalis](https://crates.io/crates/apalis)                                                       |
| The actor model (addresses, mailboxes)     | [ractor](https://crates.io/crates/ractor) / [kameo](https://crates.io/crates/kameo)             |
| Structured subsystem shutdown, no restarts | [tokio-graceful-shutdown](https://crates.io/crates/tokio-graceful-shutdown)                     |

## Two modes

| Mode             | When to use            | Lifecycle                                        |
|------------------|------------------------|--------------------------------------------------|
| `sup.run(specs)` | Tasks known upfront    | Blocks until all done or Ctrl+C                  |
| `sup.serve()`    | Tasks added at runtime | Returns a handle. You control shutdown           |

```rust,ignore
// Dynamic mode
let handle = sup.serve();

let id = handle.add(spec)?;                       // returns a TaskId
handle.cancel(id).await?;                         // cancel by identity
handle.cancel_by_label("task-name").await?;       // ...or by label
let tasks = handle.list().await;                  // Vec<(TaskId, name)>

handle.shutdown().await?;
```

## How it works

<img src="https://raw.githubusercontent.com/soltiHQ/.github/main/assets/schema/taskvisor-process.png" alt="How taskvisor works: TaskSpec wraps your async fn, the Supervisor runs and restarts it, lifecycle events go to the event bus, the final outcome is delivered once per task" width="640">

Two separate channels report what happened. The event bus is best-effort (dashed lines).
The final outcome is delivered once per task (bold line). It does not go through the bus.

Each subscriber gets its own bounded queue. A slow subscriber never blocks others or the supervisor.
Module-level concepts are documented in depth on [docs.rs](https://docs.rs/taskvisor).

## Error handling

Return these from your task to control what happens next:

| Return                           | Retryable | What happens                                                                |
|----------------------------------|-----------|-----------------------------------------------------------------------------|
| `Ok(())`                         | —         | Task completed. `RestartPolicy` decides the next step.                      |
| `Err(TaskError::fail(reason))`   | Yes       | Retryable failure. Backoff, then retry. Wrap a cause with `fail_from(err)`. |
| `panic!` in the task body        | Yes       | Caught and converted to a retryable failure.                                |
| `Err(TaskError::Timeout { .. })` | Yes       | Set automatically when the per-attempt timeout is exceeded.                 |
| `Err(TaskError::fatal(reason))`  | No        | Permanent failure. The task stops and will not restart.                     |
| `Err(TaskError::Canceled)`       | No        | Clean stop on shutdown. Not an error.                                       |

### Cancellation

Long-running tasks must observe cancellation through their `TaskContext`:

```rust,ignore
// Pattern 1: wrap every await (recommended)
ctx.run_until_cancelled(do_work()).await?;

// Pattern 2: select! for manual control
tokio::select! {
    _ = ctx.cancelled() => Err(TaskError::Canceled),
    result = do_work() => result,
}
```

## Recipes

### Restart tasks on failure with exponential backoff

```rust,no_run
use std::time::Duration;
use taskvisor::{BackoffPolicy, JitterPolicy};

// 200ms -> 400ms -> 800ms -> ... capped at 30s, with jitter.
let backoff = BackoffPolicy::exponential(Duration::from_millis(200))
    .with_max(Duration::from_secs(30))
    .with_jitter(JitterPolicy::Equal);
```

Jitter spreads retries in time. It prevents many tasks from retrying at the same moment.
`JitterPolicy::Equal` keeps each delay within `[base/2, base]` and is the recommended default.

### Run a periodic task

```rust,ignore
// Run, wait 30 seconds, run again. Forever.
// The interval starts after each completion (not a wall-clock schedule).
let spec = TaskSpec::periodic(task, Duration::from_secs(30));
```

### Set a timeout and a retry limit

```rust,ignore
// Each attempt gets 5s; the task gives up after 3 failure-driven retries.
let spec = TaskSpec::restartable(task)
    .with_timeout(Some(Duration::from_secs(5)))
    .with_max_retries(std::num::NonZeroU32::new(3).unwrap());
```

### Configure the supervisor

```rust,no_run
use std::time::Duration;
use taskvisor::{Supervisor, SupervisorConfig};

let sup = Supervisor::builder(SupervisorConfig::default())
    .with_grace(Duration::from_secs(30))   // shutdown grace period (default 60s)
    .with_timeout(Duration::from_secs(5))  // default per-attempt timeout (default: none)
    .with_max_retries(10)                  // default retry limit (default: unlimited)
    .with_max_concurrent(4)                // global concurrency limit (default: unlimited)
    .build();
```

### Graceful shutdown on Ctrl+C

Static mode (`run`) installs OS shutdown signal handlers for you.
In dynamic mode, you stop things yourself: `handle.shutdown()` cancels all tasks and waits up to `grace`.
Tasks that ignore cancellation are force-aborted after the grace period.
The outcome is reported either way.

### Run CPU-heavy work without blocking Tokio

Offload the computation to rayon and await the result through a oneshot channel — inside a supervised task.
Tokio threads stay unblocked. Taskvisor adds restart with backoff:

```rust,ignore
let (tx, rx) = oneshot::channel();
rayon::spawn(move || {
    let _ = tx.send(heavy_compute());
});

match ctx.run_until_cancelled(rx).await? {
    Ok(Ok(value)) => Ok(()),                 // use the value
    Ok(Err(e)) => Err(TaskError::fail(e)),   // supervisor retries with backoff
    Err(_) => Err(TaskError::fail("compute thread dropped the channel")),
}
```

Full program: [`examples/cpu_job.rs`](examples/cpu_job.rs).

### Send supervisor events to `tracing`

Your application must initialize `tracing` first (for example with `tracing-subscriber`).
Then add `TracingBridge` as a subscriber:

```rust,ignore
// features = ["tracing"]
let subs: Vec<Arc<dyn Subscribe>> = vec![Arc::new(TracingBridge)];
let sup = Supervisor::new(SupervisorConfig::default(), subs);
// Failures arrive as ERROR, retries as DEBUG in your existing log pipeline.
// Filter with RUST_LOG=taskvisor=warn.
```

A failing task recovering:

![taskvisor retries a failing task with backoff and recovers, shown as colored tracing logs](https://raw.githubusercontent.com/soltiHQ/.github/main/assets/demo/taskvisor-recovery.gif)

See [`examples/tracing.rs`](examples/tracing.rs) for the full program and [`examples/metrics.rs`](examples/metrics.rs) for Prometheus counters.

### Await a task's final result

```rust,ignore
// add_and_watch returns a TaskWaiter resolving to the final TaskOutcome.
let (id, waiter) = handle
    .add_and_watch(TaskSpec::once(job), Duration::from_secs(1))
    .await?;

match waiter.wait().await? {
    TaskOutcome::Completed => println!("{id} done"),
    TaskOutcome::Failed { reason, .. } => eprintln!("{id} failed: {reason}"),
    other => eprintln!("{id} ended with {other:?}"),
}
```

## Admission control *(feature `controller`)*

You submit tasks to named slots. A policy decides what happens when a slot is busy:

| Policy          | Behavior                                               | Use case               |
|-----------------|--------------------------------------------------------|------------------------|
| `Queue`         | FIFO queue. New task waits until the current one ends. | Job queue              |
| `Replace`       | Cancels the running task, starts the new one.          | Search-as-you-type     |
| `DropIfRunning` | Rejects the submission if the slot is busy.            | "One deploy at a time" |

```rust,ignore
let sup = Supervisor::builder(cfg)
    .with_controller(ControllerConfig::default())
    .build();
let handle = sup.serve();

// Await the admission outcome. A submission that is never admitted
// resolves to TaskOutcome::Rejected.
let (id, waiter) = handle.submit_and_watch(ControllerSpec::queue(spec)).await?;
let outcome = waiter.wait().await?;
```

See [`examples/slots.rs`](examples/slots.rs) and [`examples/admission.rs`](examples/admission.rs).

## Production notes

- **No unsafe.** The crate is `#![forbid(unsafe_code)]`.
- **Tested.** Unit and integration suites cover concurrency, shutdown, timeouts, and task identity. CI covers formatting, linting, tests, docs, and example builds.
- **Small footprint.** Depends on `tokio`, `tokio-util`, `thiserror`, `fastrand`. Optional: `dashmap` (controller), `tracing`.

## Examples

All examples run as-is:

```bash
cargo run --example basic
```

Add `--features tracing` or `--features controller` where the table notes it.

| Example                                         | What it shows                                                                           |
|-------------------------------------------------|-----------------------------------------------------------------------------------------|
| [basic.rs](examples/basic.rs)                   | Run one task and exit: the minimal wiring                                               |
| [worker.rs](examples/worker.rs)                 | A long-running worker that stops cleanly on Ctrl+C                                      |
| [periodic.rs](examples/periodic.rs)             | Run a job every N seconds, forever                                                      |
| [multiple.rs](examples/multiple.rs)             | Several tasks with different restart rules under one supervisor                         |
| [queue_consumer.rs](examples/queue_consumer.rs) | A message consumer that reconnects after failures                                       |
| [cpu_job.rs](examples/cpu_job.rs)               | Run CPU-heavy work on rayon, supervised, without blocking Tokio                         |
| [subscriber.rs](examples/subscriber.rs)         | React to lifecycle events with your own handler                                         |
| [tracing.rs](examples/tracing.rs)               | Send supervisor events into your logs (feature `tracing`)                               |
| [metrics.rs](examples/metrics.rs)               | Count lifecycle events as Prometheus metrics                                            |
| [dynamic.rs](examples/dynamic.rs)               | Add, cancel, and remove tasks while the app is running                                  |
| [outcomes.rs](examples/outcomes.rs)             | Wait for a task's final result: done, failed, or canceled                               |
| [slots.rs](examples/slots.rs)                   | Limit concurrency per slot: queue, replace, or drop the newcomer (feature `controller`) |
| [admission.rs](examples/admission.rs)           | Find out if your submission ran or was rejected (feature `controller`)                  |

## Performance

Run the benchmarks on your own hardware:

```bash
cargo bench                                          # all suites
cargo bench --bench lifecycle                        # task lifecycle overhead
cargo bench --bench controller --features controller # admission control
```

## Feature flags

| Feature              | What it enables                                                                       |
|----------------------|---------------------------------------------------------------------------------------|
| `controller`         | Slot-based admission control: `ControllerSpec`, `ControllerConfig`, `AdmissionPolicy` |
| `tracing`            | Built-in `TracingBridge` subscriber: events flow into your `tracing` log pipeline     |
| `logging`            | Built-in `LogWriter` subscriber: event output to stdout (demo/reference)              |
| `tokio-util-interop` | Access to the raw `CancellationToken` behind `TaskContext`, for APIs that need one    |

```toml
taskvisor = { version = "0.4", features = ["controller", "tracing"] }
```

## Contributing

Issues and pull requests are welcome. Start with the [contributing guide](https://github.com/soltiHQ/.github/blob/main/CONTRIBUTING.md).

<br>

##

<p align="center">
  <a href="https://github.com/soltiHQ">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/soltiHQ/.github/main/assets/word/solti-word-light.svg">
      <img src="https://raw.githubusercontent.com/soltiHQ/.github/main/assets/logo/solti-logo-dark.svg" alt="soltiHQ" height="128">
    </picture>
  </a>
</p>