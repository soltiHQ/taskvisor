# Taskvisor

[![Crates.io](https://img.shields.io/crates/v/taskvisor.svg)](https://crates.io/crates/taskvisor)
[![docs.rs](https://docs.rs/taskvisor/badge.svg)](https://docs.rs/taskvisor)
[![Minimum Rust 1.90](https://img.shields.io/badge/rust-1.90%2B-orange.svg)](https://rust-lang.org)
[![Apache 2.0](https://img.shields.io/badge/license-Apache2.0-blue.svg)](./LICENSE)

> **Supervise Tokio tasks with retries, graceful shutdown, reliable final outcomes, and per-key admission control.**

Taskvisor is an in-process task supervisor for Rust services.
It turns ordinary async work into a managed lifecycle with backoff, timeouts, cancellation, runtime control, and direct outcomes for watched tasks.

When work competes for the same application key, the optional controller queues it, replaces older work, or rejects it.
Conflict policy is evaluated per key; supervisor-wide limits still apply.

[Quick start](#quick-start) · [Documentation](https://solti.io/docs/taskvisor/) · [API docs](https://docs.rs/taskvisor) · [Examples](examples/README.md) · [Benchmarks](benches/README.md)

## The retry loop you stop maintaining

An illustrative Tokio worker can start with a retry loop like this:

```rust,ignore
tokio::spawn(async move {
    loop {
        if run_worker().await.is_ok() {
            break;
        }

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
});
```

This loop hard-codes the delay and has no retry limit, attempt timeout, cancellation path, shutdown coordination, or direct final outcome. 
Taskvisor owns those lifecycle rules, including configurable backoff. `TaskWaiter` delivers the final `TaskOutcome` directly for watched work. 
Your task keeps the application logic.

## Quick start

```toml
[dependencies]
taskvisor = "0.9"
tokio = { version = "1", features = ["full"] }
```

Put this in `src/main.rs`, then run `cargo run`:

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use taskvisor::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let attempts = Arc::new(AtomicU32::new(0));
    let refresh: TaskRef = TaskFn::arc(move |_ctx| {
        let attempts = Arc::clone(&attempts);
        async move {
            let attempt = attempts.fetch_add(1, Ordering::Relaxed) + 1;
            println!("attempt {attempt}");

            if attempt < 3 {
                Err(TaskError::fail("temporary failure"))
            } else {
                Ok(())
            }
        }
    });

    let spec = TaskSpec::restartable("refresh-cache", refresh)
        .with_backoff(BackoffPolicy::constant(Duration::from_millis(50)))
        .try_with_max_retries(2)?;

    let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    let handle = supervisor.serve()?;

    let waiter = handle.add(spec).watch().execute().await?;
    println!("final outcome: {:?}", waiter.wait().await?);

    handle.shutdown().await?;
    Ok(())
}
```

```text
attempt 1
attempt 2
attempt 3
final outcome: Completed
```

Each attempt gets a fresh future.
Taskvisor applies the configured backoff after the first two retryable failures.
The third attempt succeeds, and `TaskWaiter` delivers the final outcome through a direct channel outside the best-effort event path.

For the smallest static task, see [basic.rs](examples/basic.rs).
For a resident worker that stops on Ctrl+C, see [graceful_worker.rs](examples/graceful_worker.rs).

## Beyond retries

Retry is one lifecycle decision. Service code still has to stop work, report its final result, and
resolve conflicts when new work targets a resource that is already busy.

| Need                   | Taskvisor provides                                                                                                |
|------------------------|-------------------------------------------------------------------------------------------------------------------|
| Supervised lifecycle   | One-shot, retrying, or periodic tasks with backoff, jitter, retry limits, timeouts, and cooperative cancellation. |
| Runtime control        | Add, inspect, cancel, remove, and watch tasks while the service is running.                                       |
| Reliable final results | One direct `TaskOutcome` through `TaskWaiter`, separate from best-effort events.                                  |
| Per-key coordination   | Queue, replace, or reject competing submissions by controller slot.                                               |
| Typed observability    | Structured lifecycle events for logs, traces, metrics, and live diagnostics.                                      |
| Explicit limits        | Configurable bounds for queues, registered tasks, concurrent attempts, and values retained during cleanup.        |

## One key, one owner

Retries decide what happens after one attempt.
Admission decides what happens when new work conflicts with work already owned for the same resource.

```text
submission ──► controller slot
                    ├── idle ──► try registry admission
                    └── busy
                         ├── Queue ─────────► join the bounded FIFO queue
                         ├── Replace ───────► retire owner; become the next candidate
                         └── DropIfRunning ─► reject without starting
```

`Queue` preserves FIFO order.
A later `Replace` can displace the queue head while leaving the FIFO tail in place.

A task name identifies registry membership.
A controller slot groups submissions that must not overlap.
Different task names can share a slot.
Without `with_slot`, the task name is used for both roles.

```text
ControllerSpec::replace(TaskSpec::once(task_name, task))
    .with_slot(application_key)
               ▼
SupervisorHandle::submit(request).watch().execute()
               ▼
TaskWaiter::wait()
```

The `controller` feature is enabled by default.
A supervisor uses controller admission only when it is built with `SupervisorBuilder::with_controller`.

See [tenant_sync.rs](examples/tenant_sync.rs) for a complete latest-wins workflow across separate tenant slots.
The [user guide](docs/keyed-admission.md) explains queue ordering, replacement, rejection, slot identity, and controller limits.

## When Taskvisor fits

Consider Taskvisor when at least one of these is true:

- tasks are added, removed, or watched while the service is running;
- attempts need retry limits, timeouts, backoff, or coordinated cancellation;
- application logic must receive the final outcome of watched work;
- competing work for the same application key needs an explicit admission policy.

If you only need retry for one future or a small fixed set of workers, Taskvisor may be more than you need.

Taskvisor is intentionally in-process.
It is not a persistent job queue, and its runtime state does not survive process exit.

Choose a more focused tool when the main requirement is different:

| You need                                                 | Consider                                                                                          |
|----------------------------------------------------------|---------------------------------------------------------------------------------------------------|
| A small fixed set of workers with retry and cancellation | `JoinSet` or `TaskTracker`, `CancellationToken`, and [BackON](https://crates.io/crates/backon).   |
| Retry for one future                                     | [BackON](https://crates.io/crates/backon) or [tokio-retry](https://crates.io/crates/tokio-retry). |
| Durable jobs that survive process restart                | [Apalis](https://crates.io/crates/apalis) with a persistent storage backend.                      |
| Actors with addresses and mailboxes                      | [Ractor](https://crates.io/crates/ractor) or [Kameo](https://crates.io/crates/kameo).             |
| Structured subsystem shutdown without restart policies   | [tokio-graceful-shutdown](https://crates.io/crates/tokio-graceful-shutdown).                      |

## Important boundaries

Taskvisor makes its process boundary explicit:

- tasks, controller queues, task IDs, and watched outcomes do not survive process exit;
- lifecycle events are best-effort; use `TaskWaiter` when application logic needs a final result;
- cancellation starts cooperatively; synchronous task code cannot be interrupted at the grace deadline;
- shutdown stops waiting after configured deadlines, but synchronous callbacks or destructors that already started may still be running;
- `ownership_snapshot` reports retained ownership and queued or running deferred cleanup separately from task membership;
- periodic tasks use a delay after completion, not a calendar or cron schedule;
- controller slots coordinate work inside one supervisor.

Read the [full production boundaries](docs/production-boundaries.md) before deploying a service.

## Examples and documentation

The repository contains 18 complete runnable programs.

| Start here                                        | Learn                                              |
|---------------------------------------------------|----------------------------------------------------|
| [basic.rs](examples/basic.rs)                     | Run one static task.                               |
| [graceful_worker.rs](examples/graceful_worker.rs) | Stop a resident worker cooperatively.              |
| [outcomes.rs](examples/outcomes.rs)               | Await classified final outcomes.                   |
| [dynamic_tasks.rs](examples/dynamic_tasks.rs)     | Add, inspect, cancel, and remove tasks at runtime. |
| [tenant_sync.rs](examples/tenant_sync.rs)         | Keep the newest waiting revision for each tenant.  |

The [examples guide](examples/README.md) provides the complete learning path, run commands, feature flags, and stop behavior.

Use the [user guide](docs/index.md) for application workflows and production boundaries, then open the [API documentation](https://docs.rs/taskvisor) for exact contracts.
The [installation guide](docs/installation.md) lists the optional `tracing`, `logging`, `tokio-util-interop`, and `test-util` features. Use the [API documentation](https://docs.rs/taskvisor) for each integration's exact public contract.

## Benchmarks

Five Criterion suites measure lifecycle cost, batch throughput, subscriber fan-out, dynamic management, and controller paths.
Each reported case states what was timed and what remained outside the measurement.

From a cloned repository with [Task](https://taskfile.dev/) installed, run the complete suite:

```bash
task rust:test/bench
```

The [benchmark guide](benches/README.md) explains every result field.
Benchmark measurements describe the tested case and machine; they are not an application capacity promise.

## Contributing

Issues and pull requests are welcome.

Read the [contributor map](src/ARCHITECTURE.md) before changing runtime flows and the [contributing guide](https://github.com/soltiHQ/.github/blob/main/CONTRIBUTING.md) before a large change.

If Taskvisor earns a place in your stack, a GitHub star helps other Rust developers find it.

<br>

<p align="center">
  <a href="https://github.com/soltiHQ">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/soltiHQ/.github/main/assets/word/solti-word-light.svg">
      <img src="https://raw.githubusercontent.com/soltiHQ/.github/main/assets/logo/solti-logo-dark.svg" alt="soltiHQ" height="84">
    </picture>
  </a>
</p>
