---
title: Quick start
description: Run a retrying Taskvisor task and wait for its direct final outcome.
---

# Quick start

This example runs one task, retries two temporary failures, and waits for the final outcome.
It uses `serve` and `add_and_watch` because the caller needs that task's result.
`Supervisor::run` would report the shared supervisor lifecycle instead of returning one task outcome.

## Create a project

Taskvisor requires Rust 1.90 or newer.
The default Taskvisor install includes everything this example uses; no additional Taskvisor feature is required.

```sh
cargo new taskvisor-quick-start
cd taskvisor-quick-start
cargo add taskvisor@0.8
cargo add tokio@1 --features full
```

## Add the task

Replace `src/main.rs` with:

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

    let (_, waiter) = handle.add_and_watch(spec).await?;
    println!("final outcome: {:?}", waiter.wait().await?);

    handle.shutdown().await?;
    Ok(())
}
```

## Run it

Run the program:

```sh
cargo run
```

It prints:

```text
attempt 1
attempt 2
attempt 3
final outcome: Completed
```

## Understand what happened

```text
TaskFn ──► TaskSpec ──► add_and_watch ──► supervised attempts ──► TaskWaiter
                                │                                  │
                                └──────── task ID ─────────────────┘
```

`TaskFn` creates a fresh future for every attempt.
The first two attempts return retryable failures.
Taskvisor waits for the configured backoff before each retry, and the third attempt succeeds.

A retry limit counts retries after the first failed attempt.
The limit of two therefore permits the initial attempt and at most two retries.

`add_and_watch` returns a `TaskWaiter`.
Its direct in-process channel delivers the final `TaskOutcome` outside the best-effort event path.
`handle.shutdown().await` then joins the shared shutdown and cleanup workflow.

Every retry starts a new attempt.
Before returning a retryable failure for work with external side effects, make the operation safe to repeat.
Read [Make repeated attempts safe](lifecycle-policies.md#make-repeated-attempts-safe) before applying this policy to real I/O.

## Continue from here

| Next need                                 | Continue with                                                                                     |
|-------------------------------------------|---------------------------------------------------------------------------------------------------|
| Fixed or resident workers                 | [Run Taskvisor](running-and-managing.md) and [graceful_worker.rs](../examples/graceful_worker.rs) |
| Runtime-discovered work                   | [Manage tasks at runtime](managing-tasks.md) and [dynamic_tasks.rs](../examples/dynamic_tasks.rs) |
| Queue, replace, or reject work by key     | [Coordinate work by key](keyed-admission.md) and [tenant_sync.rs](../examples/tenant_sync.rs)     |
| Logs, traces, metrics, or direct outcomes | [Final outcomes and lifecycle events](outcomes-and-events.md)                                     |
| Production limits and shutdown boundaries | [Configure Taskvisor](configuration.md) and [Production boundaries](production-boundaries.md)     |
