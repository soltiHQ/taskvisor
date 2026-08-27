# Taskvisor

[![Crates.io](https://img.shields.io/crates/v/taskvisor.svg)](https://crates.io/crates/taskvisor)
[![docs.rs](https://docs.rs/taskvisor/badge.svg)](https://docs.rs/taskvisor)
[![Minimum Rust 1.90](https://img.shields.io/badge/rust-1.90%2B-orange.svg)](https://rust-lang.org)
[![Apache 2.0](https://img.shields.io/badge/license-Apache2.0-blue.svg)](./LICENSE)

> **One owner per key. Queue conflicting work, keep the newest replacement at the queue head, or reject the incoming submission — and let watched callers await final outcomes directly.**

Taskvisor is an in-process task supervisor for Tokio services.
It manages retries, per-attempt timeouts, cooperative cancellation, coordinated shutdown, and a direct outcome channel for watched work.
Its optional controller adds queue, replace, or reject admission by application key.

[The problem](#the-problem) · [Quick start](#quick-start) · [Documentation](https://solti.io/docs/taskvisor/) · [API docs](https://docs.rs/taskvisor) · [Examples](examples/README.md)

## The problem

Two requests to sync the same tenant arrive. The first one is still running. What happens to the second?

Application code can build this around a `HashMap<Key, JoinHandle>` and a `Mutex`.
Matching Taskvisor's lifecycle contract also requires explicit handling for panics, per-attempt timeouts, cancellation, shutdown, and the distinction between logical outcome and physical task release.

Taskvisor answers it with a slot:

```text
submission ──► slot "tenant-42"
                    ├── idle ──► start now
                    └── busy
                         ├── Queue ─────────► wait in FIFO order
                         ├── Replace ───────► request owner stop; create or replace queue head
                         └── DropIfRunning ─► reject without starting
```

A slot has at most one owner. Different slots can proceed independently, subject to supervisor-wide limits.
A slot remains occupied through admission, task lifetime, terminal reporting, and physical actor release; a logical outcome alone does not release it.
After successful watched intake, the caller uses `TaskWaiter` to await controller rejection or the admitted task's final `TaskOutcome`.
Unexpected direct-channel closure is reported as `OutcomeUnavailable`.

## Quick start

```toml
[dependencies]
taskvisor = "0.9"
tokio = { version = "1", features = ["full"] }
```

Two revisions for the same tenant. They must never overlap:

```rust
use std::sync::{Arc, Mutex};
use std::time::Duration;
use taskvisor::prelude::*;

fn sync_revision(log: Arc<Mutex<Vec<String>>>, rev: u32) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(move |_ctx| {
        let log = Arc::clone(&log);
        async move {
            log.lock().unwrap().push(format!("rev{rev} start"));
            tokio::time::sleep(Duration::from_millis(20)).await;
            log.lock().unwrap().push(format!("rev{rev} done"));
            Ok(())
        }
    });
    TaskSpec::once(format!("sync-rev{rev}"), task)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let log: Arc<Mutex<Vec<String>>> = Arc::default();

    let supervisor = Supervisor::builder(SupervisorConfig::default())
        .with_controller(ControllerConfig::default())
        .build();
    let handle = supervisor.serve()?;

    let first = handle
        .submit(ControllerSpec::queue(sync_revision(Arc::clone(&log), 1)).with_slot("tenant-42"))
        .watch()
        .execute()
        .await?;
    let second = handle
        .submit(ControllerSpec::queue(sync_revision(Arc::clone(&log), 2)).with_slot("tenant-42"))
        .watch()
        .execute()
        .await?;

    println!("rev1: {:?}", first.wait().await?);
    println!("rev2: {:?}", second.wait().await?);
    println!("order: {:?}", log.lock().unwrap());

    handle.shutdown().await?;
    Ok(())
}
```

```text
rev1: Completed
rev2: Completed
order: ["rev1 start", "rev1 done", "rev2 start", "rev2 done"]
```

The two revisions never interleave, and both watched callers get a typed outcome.
With `replace`, a submission accepted into a busy slot requests the owner to stop and creates or replaces the queue head.
The replacement starts only after physical owner release; a newer `replace` can supersede it, and registry admission can still reject it.
With `drop_if_running`, a submission is rejected without starting its task body if the slot is busy when the controller evaluates the policy.

See [tenant_sync.rs](examples/tenant_sync.rs) for the complete latest-wins workflow across separate tenant slots.

## What else you get

Keyed admission is one reason to reach for Taskvisor. The same supervisor also provides:

| Need                   | Taskvisor provides                                                                                         |
|------------------------|------------------------------------------------------------------------------------------------------------|
| Direct final results   | Successful watched intake returns `TaskWaiter` on a process-local direct channel, separate from best-effort events. |
| Supervised lifecycle   | One-shot, retrying, or periodic tasks with backoff, jitter, retry limits, per-attempt timeouts, and cancellation. |
| Runtime control        | Add, inspect, cancel, remove, and watch tasks while the service is running.                                |
| Coordinated shutdown   | One shared shutdown with a grace deadline, optional OS signal handling, and a reported result.             |
| Typed observability    | Structured lifecycle events for logs, traces, metrics, and live diagnostics.                               |
| Explicit limits        | Configurable bounds for queues, registered tasks, concurrent attempts, and values retained during cleanup. |

The crate forbids unsafe Rust with `#![forbid(unsafe_code)]`.

## When Taskvisor is the wrong tool

Taskvisor is in-process. It is not a persistent job queue, and its state does not survive process exit.
Without keyed conflicts, direct `add` still supports managed retries, runtime control, coordinated shutdown, and watched outcomes.
If those contracts are also unnecessary, a smaller primitive may be sufficient.

| You need                                                 | Consider                                                                                          |
|----------------------------------------------------------|---------------------------------------------------------------------------------------------------|
| Retry for one future                                     | [BackON](https://crates.io/crates/backon) or [tokio-retry](https://crates.io/crates/tokio-retry). |
| A small fixed set of workers with retry and cancellation | `JoinSet` or `TaskTracker`, `CancellationToken`, and [BackON](https://crates.io/crates/backon).   |
| Durable jobs that survive process restart                | [Apalis](https://crates.io/crates/apalis) with a persistent storage backend.                      |
| Actors with addresses and mailboxes                      | [Ractor](https://crates.io/crates/ractor) or [Kameo](https://crates.io/crates/kameo).             |
| Subsystem shutdown without restart policies              | [tokio-graceful-shutdown](https://crates.io/crates/tokio-graceful-shutdown).                      |

## Boundaries worth knowing early

- Cancellation is cooperative. Synchronous task code cannot be interrupted at the grace deadline.
- Lifecycle events are best-effort and can be dropped. Use `TaskWaiter` when correctness depends on a result.
- Periodic tasks use a delay after completion, not a cron schedule or missed-run recovery.
- Controller slots coordinate work inside one supervisor, not across processes.

Read the [full production boundaries](docs/production-boundaries.md) before deploying a service.

## Learn more

| Where                                          | What it covers                                                                        |
|------------------------------------------------|---------------------------------------------------------------------------------------|
| [Examples](examples/README.md)                 | 18 complete runnable programs, from [basic.rs](examples/basic.rs) to keyed admission. |
| [User guide](docs/index.md)                    | Workflows, configuration, cancellation, outcomes, and production limits.              |
| [API documentation](https://docs.rs/taskvisor) | Exact signatures, error variants, and edge-case contracts.                            |
| [Benchmark guide](benches/README.md)           | What each measured case includes, and what it deliberately leaves out.                |

The default-enabled `controller` feature exposes keyed admission, but each supervisor installs the controller explicitly with `SupervisorBuilder::with_controller`.
Optional integrations: `tracing`, `logging`, `tokio-util-interop`, `test-util`.
The [installation guide](docs/installation.md) explains every feature.

## Contributing

Issues and pull requests are welcome.
Read the [contributor map](src/ARCHITECTURE.md) before changing runtime flows, and the [contributing guide](https://github.com/soltiHQ/.github/blob/main/CONTRIBUTING.md) before a large change.

With [Task](https://taskfile.dev/) installed, run a core subset of the checks used by CI:

```bash
task ci/fmt && task ci/clippy && task ci/test/unit && task ci/test/integration
```

`task ci` lists every repository CI task, including `ci/audit`, `ci/docs`, and `ci/publish/dry-run`.
`task rust:test/bench` runs the benchmark suite.

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
