# Taskvisor

[![Crates.io](https://img.shields.io/crates/v/taskvisor.svg)](https://crates.io/crates/taskvisor)
[![docs.rs](https://docs.rs/taskvisor/badge.svg)](https://docs.rs/taskvisor)
[![Minimum Rust 1.90](https://img.shields.io/badge/rust-1.90%2B-orange.svg)](https://rust-lang.org)
[![Apache 2.0](https://img.shields.io/badge/license-Apache2.0-blue.svg)](./LICENSE)

> In-process task supervision for Tokio - from long-running workers to one-shot jobs.

Write ordinary async code. Taskvisor adds restart and backoff, cooperative shutdown, dynamic task management, typed lifecycle events, reliable final outcomes, and optional per-slot admission control.

[Quick start](#quick-start) · [API docs](https://docs.rs/taskvisor) · [Examples](#examples) · [Production limits](#production-limits)

## The loop you stop writing

Long-running services often grow a loop like this:

```rust,ignore
tokio::spawn(async move {
    loop {
        match run_worker().await {
            Ok(()) => break,
            Err(error) => {
                eprintln!("worker failed: {error}; retrying in 1s");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
});
```

It has a fixed retry delay, no jitter, no graceful shutdown, and no typed lifecycle observability.

Once the task is defined, its supervision policy becomes one declaration:

```rust,ignore
supervisor
    .run(vec![TaskSpec::restartable(worker)])
    .await?;
```

Taskvisor owns the lifecycle machinery. Your task keeps the application logic.

## Quick start

```toml
[dependencies]
taskvisor = "0.6"
tokio = { version = "1", features = ["full"] }
```

Put this in `src/main.rs`, then run `cargo run`. The task fails twice, Taskvisor retries it with backoff, and the third attempt succeeds.

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use taskvisor::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let attempts = Arc::new(AtomicU32::new(0));
    let flaky = TaskFn::arc("flaky", move |_ctx| {
        let attempts = Arc::clone(&attempts);
        async move {
            let attempt = attempts.fetch_add(1, Ordering::Relaxed) + 1;
            println!("attempt {attempt}");

            if attempt < 3 {
                Err(TaskError::fail(format!("temporary failure #{attempt}")))
            } else {
                Ok(())
            }
        }
    });

    let spec = TaskSpec::restartable(flaky)
        .with_backoff(BackoffPolicy::constant(Duration::from_millis(50)));

    let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    supervisor.run(vec![spec]).await?;
    Ok(())
}
```

```text
attempt 1
attempt 2
attempt 3
```

Every attempt gets a fresh future. A retryable failure follows the configured backoff; a successful `restartable` task stops. `Supervisor::run` returns only after lifecycle cleanup finishes.

For a resident worker that runs until Ctrl+C, see [worker.rs](examples/worker.rs). For a reconnecting queue consumer, see [queue_consumer.rs](examples/queue_consumer.rs).

## Why Taskvisor?

Restart and backoff are the baseline. Taskvisor also provides:

- **Cooperative shutdown with a deadline.** Tasks observe `TaskContext`; tasks that miss the grace period are force-aborted.
- **Reliable final outcomes.** `TaskWaiter` reports how watched work ended even when best-effort events are dropped.
- **Typed lifecycle events.** Logs, metrics, traces, and live status consume one structured event model.
- **Dynamic management.** Add, list, cancel, remove, and watch tasks through `SupervisorHandle`.
- **Admission control.** The optional controller applies `Queue`, `Replace`, or `DropIfRunning` per named slot.
- **Explicit limits.** Configure per-attempt timeout, retry budget, global concurrency, and bounded queues.

`JoinSet` and `TaskTracker` help own and join spawned futures. Taskvisor owns the restart policy and the task's complete lifecycle contract.

It is a good fit for queue consumers, pollers, sync loops, connection keepers, periodic work, and one-shot in-process jobs that need admission or a reliable outcome.

When the primary requirement is different, use a more specialized tool:

| You need                                        | Better fit                                                                                       |
|-------------------------------------------------|--------------------------------------------------------------------------------------------------|
| Retry one future                                | [backon](https://crates.io/crates/backon) or [tokio-retry](https://crates.io/crates/tokio-retry) |
| Persist and recover jobs after a process restart | [apalis](https://crates.io/crates/apalis)                                                        |
| Actors with addresses and mailboxes             | [ractor](https://crates.io/crates/ractor) or [kameo](https://crates.io/crates/kameo)             |
| Structured subsystem shutdown without restarts  | [tokio-graceful-shutdown](https://crates.io/crates/tokio-graceful-shutdown)                      |

## Core model

Four types form the main API:

| Type               | Purpose                                                    |
|--------------------|------------------------------------------------------------|
| `TaskFn` or `Task` | The async work. A new future is created for every attempt. |
| `TaskSpec`         | Restart policy, backoff, timeout, and retry limit.         |
| `Supervisor`       | Owns task lifecycle, shutdown, and event delivery.         |
| `SupervisorHandle` | Adds, removes, cancels, and watches tasks at runtime.      |

<p align="center">
  <img src="https://raw.githubusercontent.com/soltiHQ/.github/main/assets/schema/taskvisor-process.png" alt="Taskvisor core lifecycle: the Supervisor runs one task attempt, then either schedules another after failure backoff or an optional interval, or produces a final outcome for watched tasks" width="556">
</p>

Retries for one task run in sequence. Two attempts for the same `TaskId` never run at the same time. Active task names must be unique; the name can be reused after terminal cleanup, with a new `TaskId`.

There are two runtime modes:

| Mode                    | Use it when                | Shutdown owner                                  |
|-------------------------|----------------------------|-------------------------------------------------|
| `supervisor.run(specs)` | Tasks are known at startup | Taskvisor waits for completion or an OS signal. |
| `supervisor.serve()`    | Tasks are added at runtime | Your code calls `handle.shutdown().await`.      |

`Supervisor::run(...).await == Ok(())` means the supervisor lifecycle and cleanup completed successfully. It does not mean that every task succeeded. Use a watched outcome when application logic needs that answer.

## Choose task behavior

The named constructors cover the common cases:

| Constructor                       | After `Ok(())`               | After a retryable failure |
|-----------------------------------|------------------------------|---------------------------|
| `TaskSpec::once(task)`            | Stop                         | Stop                      |
| `TaskSpec::restartable(task)`     | Stop                         | Retry with backoff        |
| `TaskSpec::periodic(task, every)` | Wait `every`, then run again | Retry with backoff        |

Fatal errors and cancellation always stop the task. A periodic interval begins after a successful attempt finishes; it is not a wall-clock or cron schedule.

| Task result                            | Meaning                                                                  |
|----------------------------------------|--------------------------------------------------------------------------|
| `Ok(())`                               | The attempt succeeded. The restart policy decides what follows.          |
| `Err(TaskError::fail(reason))`         | Retryable failure. Use `fail_from(error)` to preserve the source error.  |
| `Err(TaskError::fatal(reason))`        | Permanent failure. Do not restart.                                       |
| `Err(TaskError::Canceled)`             | Cooperative stop. Treat it as cancellation, not failure.                 |
| Attempt timeout                        | Taskvisor creates a retryable `TaskError::Timeout`.                      |
| Panic with panic unwinding enabled     | Taskvisor catches it and creates a retryable failure.                    |

A retry limit counts retries after the first failed attempt. `max_retries = 3` therefore allows at most four attempts when every attempt fails.

```rust
use std::num::NonZeroU32;
use std::time::Duration;
use taskvisor::{BackoffPolicy, JitterPolicy, TaskRef, TaskSpec};

fn supervised(task: TaskRef) -> TaskSpec {
    TaskSpec::restartable(task)
        .with_backoff(
            BackoffPolicy::exponential(Duration::from_millis(200))
                .with_max(Duration::from_secs(30))
                .with_jitter(JitterPolicy::Equal),
        )
        .with_timeout(Duration::from_secs(10))
        .with_max_retries(NonZeroU32::new(3).unwrap())
}
```

Equal jitter chooses a real delay between half of the current base delay and the full base delay, spreading simultaneous retries over time. Per-task settings override values inherited from `TaskDefaults`.

## Cancellation and shutdown

Cancellation is cooperative first. A long-running task must observe `TaskContext`:

```rust
use taskvisor::{TaskContext, TaskError};

async fn do_work() -> Result<(), TaskError> {
    // Application work goes here.
    Ok(())
}

async fn run_one_operation(ctx: &TaskContext) -> Result<(), TaskError> {
    ctx.run_until_cancelled(do_work()).await?
}

async fn run_with_more_branches(ctx: &TaskContext) -> Result<(), TaskError> {
    tokio::select! {
        _ = ctx.cancelled() => Err(TaskError::Canceled),
        result = do_work() => result,
    }
}
```

The joined shutdown path:

1. Closes admission for new work.
2. Sends cancellation to active tasks.
3. Waits for the configured grace period.
4. Force-aborts tasks that did not stop.
5. Drains subscriber queues for their separate shutdown timeout.

Call `handle.shutdown().await` to wait for cleanup and receive its result. Dropping the last public owner starts non-blocking cancellation but cannot report cleanup errors.

Dynamic management uses `TaskId`:

```rust,ignore
let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
let handle = supervisor.serve();

let id = handle.add(TaskSpec::restartable(worker)).await?;
let registered = handle.list().await;
let stopped = handle.cancel(id).await?;

println!("registered={}, stopped={stopped}", registered.len());
handle.shutdown().await?;
```

`add().await?` means the registry accepted the task, not that the task completed. Regular management methods wait for bounded queue capacity; their `try_*` forms fail fast when a queue is full. See [dynamic.rs](examples/dynamic.rs) for a complete program.

## Events and outcomes

Taskvisor has two result paths with different contracts:

| Path                               | Delivery                                      | Use it for                                              |
|------------------------------------|-----------------------------------------------|---------------------------------------------------------|
| `Event` through `Subscribe`        | Best-effort                                   | Logs, metrics, traces, and live status.                 |
| `TaskOutcome` through `TaskWaiter` | One final result, separate from the event bus | Business logic that must know how a watched task ended. |

Use `add_and_watch` when the final result matters:

```rust
use taskvisor::{RuntimeError, SupervisorHandle, TaskOutcome, TaskRef, TaskSpec};

async fn wait_for_task(
    handle: &SupervisorHandle,
    job: TaskRef,
) -> Result<(), RuntimeError> {
    let (id, waiter) = handle
        .add_and_watch(TaskSpec::once(job))
        .await?;

    match waiter.wait().await? {
        TaskOutcome::Completed => println!("{id} completed"),
        TaskOutcome::Failed { reason, .. } => eprintln!("{id} failed: {reason}"),
        TaskOutcome::Canceled => eprintln!("{id} was canceled"),
        other => eprintln!("{id} ended with {other:?}"),
    }
    Ok(())
}
```

Events carry a process-local sequence number and, where relevant, task identity, attempt, duration, reason, timeout, delay, and exit code. Stable string labels are available for telemetry.

Each subscriber has its own bounded FIFO queue. Its synchronous callback runs on Tokio's blocking pool. A slow subscriber cannot block publishers, but its queue may fill and lose events. Keep callbacks short and forward async work to another channel.

See [subscriber.rs](examples/subscriber.rs), the `TracingBridge` in [tracing.rs](examples/tracing.rs), and the Prometheus counters in [metrics.rs](examples/metrics.rs).

## Admission control (feature: `controller`)

Enable the controller on the dependency:

```toml
taskvisor = { version = "0.6", features = ["controller"] }
```

The controller groups submissions into named slots. At most one task can occupy a slot; different slots can run concurrently.

| Policy          | Busy-slot behavior                                                           | Typical use                              |
|-----------------|------------------------------------------------------------------------------|------------------------------------------|
| `Queue`         | Wait in a bounded FIFO queue.                                                | Ordered work for one resource.           |
| `Replace`       | Retire the current owner and replace the queue head with the new submission. | Work where the next value must be fresh. |
| `DropIfRunning` | Reject the new submission.                                                   | Work that must not overlap.              |

<p align="center">
  <img src="https://raw.githubusercontent.com/soltiHQ/.github/main/assets/schema/taskvisor-controller.png" alt="Controller admission: an idle slot tries registry admission; a busy slot queues work when capacity is available, requests owner retirement and sets or replaces the queue head, or rejects the submission according to its policy" width="968">
</p>

The slot defaults to the task name. Use `with_slot` to place differently named tasks in the same lane:

```rust,ignore
use taskvisor::prelude::*;

async fn submit_to_slot(
    handle: &SupervisorHandle,
    job: TaskRef,
) -> Result<TaskOutcome, Box<dyn std::error::Error>> {
    let request = ControllerSpec::queue(TaskSpec::once(job))
        .with_slot("customer-42");
    let (_id, waiter) = handle.submit_and_watch(request).await?;
    Ok(waiter.wait().await?)
}
```

`submit().await?` confirms controller intake; admission happens later. `submit_and_watch` returns a final outcome. Work that never starts resolves to `TaskOutcome::Rejected`.

Queue depth is bounded per slot. `Replace` changes only the queue head; FIFO items behind it remain queued. `controller_snapshot()` returns a best-effort, non-transactional view of slot status and queue depth.

See [slots.rs](examples/slots.rs) and [admission.rs](examples/admission.rs) for complete programs.

## Configuration

Runtime limits and task defaults are separate:

```rust
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::Arc;
use std::time::Duration;
use taskvisor::{Supervisor, SupervisorConfig, TaskDefaults};

fn configured_supervisor() -> Arc<Supervisor> {
    let runtime = SupervisorConfig::default()
        .with_grace(Duration::from_secs(30))
        .with_subscriber_shutdown_timeout(Duration::from_secs(5))
        .with_max_concurrent(NonZeroUsize::new(16));

    let tasks = TaskDefaults::default()
        .with_timeout(Duration::from_secs(20))
        .with_max_retries(NonZeroU32::new(5).unwrap());

    Supervisor::builder(runtime)
        .with_task_defaults(tasks)
        .build()
}
```

Main defaults:

| Setting                         | Default                                         |
|---------------------------------|-------------------------------------------------|
| Graceful task shutdown          | 60 seconds                                      |
| Subscriber drain                | 5 seconds, shared by all subscriber queues      |
| Global task-attempt concurrency | Unlimited                                       |
| Event bus capacity              | 1024                                            |
| Registry command capacity       | 1024                                            |
| Restart policy                  | On failure                                      |
| Failure backoff                 | Exponential: 200 ms to 30 seconds, equal jitter |
| Attempt timeout                 | None                                            |
| Failure retry limit             | Unlimited                                       |

Capacity types are non-zero where zero would make the runtime unusable. Checked `try_with_*` setters accept raw values.

## Production limits

Taskvisor defines an in-process lifecycle. Keep these boundaries explicit:

- Events are best-effort. Do not use them as a durable audit log.
- Watched outcomes are not durable after the process exits.
- Cancellation depends on the task reaching an await point that observes `TaskContext`. Force-abort cannot stop synchronous code that blocks a runtime thread.
- Subscriber callbacks may still run on Tokio's blocking pool after their drain deadline. Tokio runtime shutdown may wait for such callbacks.
- Periodic tasks use an interval after completion. They do not provide calendar scheduling or missed-run recovery.
- The controller coordinates tasks inside one supervisor.
- With `panic = "unwind"`, Taskvisor catches task-future panics. It cannot recover from `panic = "abort"`, process aborts, memory exhaustion, or failures outside the process.

For a service deployment, call the joined shutdown path, make resident tasks cancellation-aware, set finite timeouts and retry limits where endless retry is unsafe, monitor lifecycle failures and overflow, and use watched outcomes for decisions that depend on completion.

The crate forbids unsafe Rust with `#![forbid(unsafe_code)]`.

## Feature flags

Taskvisor has no default features. The core depends on `tokio`, `tokio-util`, `thiserror`, and `fastrand`.

| Feature              | Adds                                                          |
|----------------------|---------------------------------------------------------------|
| `controller`         | Slot-based admission control; adds `dashmap`.                 |
| `tracing`            | `TracingBridge` for the `tracing` ecosystem.                  |
| `logging`            | `LogWriter`, a simple event writer for demos and small tools. |
| `tokio-util-interop` | Access to the raw cancellation token in `TaskContext`.        |
| `test-util`          | Helpers for code that integrates with Taskvisor.              |

```toml
taskvisor = { version = "0.6", features = ["controller", "tracing"] }
```

## Examples

From a cloned repository checkout, run the smallest example with:

```bash
cargo run --example basic
```

| Example                                         | What it shows                                            |
|-------------------------------------------------|----------------------------------------------------------|
| [basic.rs](examples/basic.rs)                   | One task, one run, one exit.                             |
| [worker.rs](examples/worker.rs)                 | A long-running worker with graceful cancellation.        |
| [periodic.rs](examples/periodic.rs)             | Repeated execution after an interval.                    |
| [multiple.rs](examples/multiple.rs)             | Several restart policies in one supervisor.              |
| [queue_consumer.rs](examples/queue_consumer.rs) | Reconnect after a consumer failure.                      |
| [cpu_job.rs](examples/cpu_job.rs)               | Supervise CPU-heavy work without blocking Tokio workers. |
| [subscriber.rs](examples/subscriber.rs)         | Handle typed lifecycle events.                           |
| [tracing.rs](examples/tracing.rs)               | Forward events to `tracing` (`tracing` feature).         |
| [metrics.rs](examples/metrics.rs)               | Build Prometheus counters from events.                   |
| [dynamic.rs](examples/dynamic.rs)               | Add, list, cancel, and remove tasks at runtime.          |
| [outcomes.rs](examples/outcomes.rs)             | Await the final result of a task.                        |
| [slots.rs](examples/slots.rs)                   | Compare controller policies (`controller` feature).      |
| [admission.rs](examples/admission.rs)           | Observe admission and rejection (`controller` feature).  |

## Benchmarks

The repository includes Criterion suites for lifecycle, throughput, subscriber fan-out, dynamic management, and controller paths:

```bash
cargo bench
cargo bench --bench controller --features controller
```

## Contributing

Issues and pull requests are welcome. Read the [contributing guide](https://github.com/soltiHQ/.github/blob/main/CONTRIBUTING.md) before a large change.

If Taskvisor earns a place in your stack, a GitHub star helps other Rust developers find it.

Taskvisor is licensed under [Apache-2.0](LICENSE).

<br>

<p align="center">
  <a href="https://github.com/soltiHQ">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/soltiHQ/.github/main/assets/word/solti-word-light.svg">
      <img src="https://raw.githubusercontent.com/soltiHQ/.github/main/assets/logo/solti-logo-dark.svg" alt="soltiHQ" height="84">
    </picture>
  </a>
</p>
