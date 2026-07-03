# Taskvisor

[![Crates.io](https://img.shields.io/crates/v/taskvisor.svg)](https://crates.io/crates/taskvisor)
[![docs.rs](https://docs.rs/taskvisor/badge.svg)](https://docs.rs/taskvisor)
[![Minimum Rust 1.90](https://img.shields.io/badge/rust-1.90%2B-orange.svg)](https://rust-lang.org)
[![Apache 2.0](https://img.shields.io/badge/license-Apache2.0-blue.svg)](./LICENSE)

> Task supervisor for Tokio: restarts background tasks on failure with exponential backoff, graceful shutdown, and lifecycle events.

You write the task as a plain async fn. Taskvisor keeps it alive: it restarts tasks on failure with backoff and jitter, stops everything cleanly on shutdown, and narrates every step through typed events.

## The loop you stop writing

Every long-running service grows this code: 

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

With taskvisor the same worker is one line of policy:

```rust,ignore
sup.run(vec![TaskSpec::restartable(worker)]).await?;
// Restart on failure, backoff with jitter, Ctrl+C handling: included.
```

## Quick start

```toml
[dependencies]
taskvisor = "0.4"
tokio = { version = "1", features = ["full"] }
```

A worker that polls every 5 seconds, restarts on failure, and stops cleanly on Ctrl+C:

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

A task fails twice, then succeeds. Taskvisor retries it with backoff and publishes an event for every step. A small subscriber prints them, and you *see* the supervision happen:

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
  actor_exhausted (task=flaky)      # OnFailure + success = done
  task_removed (task=flaky)
```

The same recovery live, narrated through the `tracing` feature:

![taskvisor retries a failing task with backoff and recovers, shown as colored tracing logs](https://raw.githubusercontent.com/soltiHQ/.github/main/assets/demo/taskvisor-recovery.gif)

Restart, backoff, and an event for every step, without writing a retry loop.

## Why taskvisor?

Most supervision crates stop at restart + backoff. Taskvisor adds the layers a production service actually needs:

|    | Feature                 | What you get                                                                |
|:--:|-------------------------|-----------------------------------------------------------------------------|
| 🎯 | **Guaranteed outcomes** | `await` a task's final `TaskOutcome`, even if events are dropped under load |
| 📡 | **Typed event stream**  | Every start, failure, and backoff on a bus; `tracing` bridge built in       |
| 🚦 | **Admission control**   | Slot policies (Queue / Replace / DropIfRunning) for "one deploy at a time"  |
| 🔁 | **Restart policies**    | `Never`, `OnFailure`, `Always`, or `TaskSpec::periodic` — chosen per task   |
| ⏳  | **Backoff with jitter** | `exponential` / `constant` presets with cap, floor, and four jitter modes   |
| 🚧 | **Limits**              | Per-attempt timeout, max retries, global concurrency semaphore              |

Taskvisor is not a replacement for tokio or tower.
It works one level higher: you write the task, taskvisor runs it and tells you what happened.

It is also not an actor framework: no addressable actors, no mailboxes, no message passing.
Your existing `async fn` is the unit of supervision.

## When to use taskvisor

Reach for it when you have **resident background tasks** that must stay up for the life of the process — queue consumers, pollers, sync loops, connection keepers, periodic jobs — and you want to restart tasks on failure, observe them through events, and manage them (add / remove / await) at runtime.

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
| `sup.serve()`    | Tasks added at runtime | Returns `SupervisorHandle`, you control shutdown |

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

```text
your async fn ──► TaskSpec (restart + backoff + timeout) ──► Supervisor
                                                                │  runs attempts,
                                                                │  applies policies
                                                                ▼
                                                        typed Event bus
                                                                │
                                        ┌───────────────────────┼──────────────────┐
                                        ▼                       ▼                  ▼
                                  your Subscribe          TracingBridge      guaranteed outcome
                                  (metrics, alerts)       (log pipeline)     (add_and_watch)
```

<!-- TODO: replace the ASCII above with the designed scheme from soltiHQ/.github/assets when ready -->



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

```rust,ignore
// Static mode: run() installs SIGINT/SIGTERM handlers for you.
sup.run(specs).await?;

// Dynamic mode: you decide when to stop.
let handle = sup.serve();
// ...
handle.shutdown().await?; // cancel all, wait up to `grace`, then force-abort
```

Tasks that ignore cancellation are force-aborted after the grace period.
The outcome is reported either way.

### Send supervisor events to `tracing`

```rust,ignore
// features = ["tracing"]
let subs: Vec<Arc<dyn Subscribe>> = vec![Arc::new(TracingBridge)];
let sup = Supervisor::new(SupervisorConfig::default(), subs);
// Failures arrive as ERROR, retries as DEBUG — in your existing log pipeline.
// Filter with RUST_LOG=taskvisor=warn.
```

See [`examples/tracing.rs`](examples/tracing.rs) for the full program and [`examples/metrics.rs`](examples/metrics.rs) for Prometheus counters.

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

## Admission control *(feature = `controller`)*

Tasks submit to named slots. The policy decides what happens when a slot is busy:

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

// Await the admission outcome. A never-admitted submission
// resolves to TaskOutcome::Rejected — no lost tasks.
let (id, waiter) = handle.submit_and_watch(ControllerSpec::queue(spec)).await?;
let outcome = waiter.wait().await?;
```

See [`examples/slots.rs`](examples/slots.rs) and [`examples/admission.rs`](examples/admission.rs).

## Production notes

- **No unsafe.** The crate is `#![forbid(unsafe_code)]`.
- **Tested.** ~250 unit tests plus 8 integration suites (concurrency, shutdown, timeout, identity, ...). CI runs fmt, clippy per feature combination, unit and integration tests, and per-example builds on every PR.
- **Small footprint.** Four dependencies: `tokio`, `tokio-util`, `thiserror`, `fastrand`. Optional: `dashmap` (controller), `tracing`.
- **MSRV 1.90.** MSRV bumps happen only in minor releases.
- **Pre-1.0.** Breaking changes land in minor versions (0.x → 0.x+1) and are listed in the [release notes](https://github.com/soltiHQ/taskvisor/releases).

## Performance

Run the benchmarks on your own hardware:

```bash
cargo bench                                          # all suites
cargo bench --bench lifecycle                        # task lifecycle overhead
cargo bench --bench controller --features controller # admission control
```

## Examples

Twelve runnable, tutorial-style examples, ordered as a learning path:

```bash
cargo run --example basic
cargo run --example worker
cargo run --example periodic
cargo run --example multiple
cargo run --example queue_consumer
cargo run --example subscriber
cargo run --example metrics
cargo run --example dynamic
cargo run --example outcomes
cargo run --example tracing --features tracing
cargo run --example slots --features controller
cargo run --example admission --features controller
```

| Example                                         | What it shows                                                  |
|-------------------------------------------------|----------------------------------------------------------------|
| [basic.rs](examples/basic.rs)                   | Minimal hello-world, one task runs once                        |
| [worker.rs](examples/worker.rs)                 | Long-running worker with graceful Ctrl+C shutdown              |
| [periodic.rs](examples/periodic.rs)             | Periodic task via `TaskSpec::periodic`                         |
| [multiple.rs](examples/multiple.rs)             | Three tasks with different policies and backoff                |
| [queue_consumer.rs](examples/queue_consumer.rs) | Broker consumer with reconnect + restart-on-failure            |
| [subscriber.rs](examples/subscriber.rs)         | Custom `Subscribe` implementation                              |
| [tracing.rs](examples/tracing.rs)               | Supervisor events in your `tracing` log pipeline               |
| [metrics.rs](examples/metrics.rs)               | Prometheus counters from lifecycle events                      |
| [dynamic.rs](examples/dynamic.rs)               | `serve()` + `SupervisorHandle`: add/remove/cancel at runtime   |
| [outcomes.rs](examples/outcomes.rs)             | `add_and_watch`: await a task's final `TaskOutcome`            |
| [slots.rs](examples/slots.rs)                   | Controller admission policies: Queue, Replace, DropIfRunning   |
| [admission.rs](examples/admission.rs)           | `submit_and_watch`: await admission outcome (incl. `Rejected`) |

## Feature flags

| Feature      | What it enables                                                                       |
|--------------|---------------------------------------------------------------------------------------|
| `controller` | Slot-based admission control: `ControllerSpec`, `ControllerConfig`, `AdmissionPolicy` |
| `tracing`    | Built-in `TracingBridge` subscriber — events flow into your `tracing` log pipeline    |
| `logging`    | Built-in `LogWriter` subscriber — event output to stdout (demo/reference)             |

```toml
taskvisor = { version = "0.4", features = ["controller", "tracing"] }
```

## Roadmap

Taskvisor is the supervision core of *Solti*, a larger task-orchestration toolkit in development (subprocess execution, HTTP/gRPC API, dashboards). Taskvisor stands on its own today; the rest is future work.

## Contributing

Found a bug? Have an idea? [Open an issue](https://github.com/soltiHQ/taskvisor/issues) or send a pull request.

---

<p>
  <i>Part of <a href="https://github.com/soltiHQ">soltiHQ</a>: open-source tooling for supervised background tasks.</i>
</p>

<p>
  <a href="https://github.com/soltiHQ">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/soltiHQ/.github/main/assets/logo/solti-logo-light.svg">
      <img src="https://raw.githubusercontent.com/soltiHQ/.github/main/assets/logo/solti-logo-dark.svg" alt="soltiHQ" height="34">
    </picture>
  </a>
</p>