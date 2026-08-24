---
title: Quick start
description: Run a retrying Taskvisor task and wait for its direct final outcome.
---

# Quick start

This example runs one task, retries two temporary failures, and waits for the final outcome.

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

`TaskFn` creates a fresh future for every attempt.
The first two attempts return retryable failures.
Taskvisor waits for the configured backoff before each retry, and the third attempt succeeds.

A retry limit counts retries after the first failed attempt.
The limit of two therefore permits the initial attempt and at most two retries.

`add_and_watch` returns a `TaskWaiter`.
Its direct in-process channel delivers the final `TaskOutcome` outside the best-effort event path.
`handle.shutdown().await` then joins the shared shutdown and cleanup workflow.

## Continue from here

- [Define a task](defining-tasks.md) explains `TaskFn`, `Task`, and shared task values.
- [Choose task behavior](lifecycle-policies.md) covers restart policies, backoff, timeouts, and retry limits.
- [Manage tasks at runtime](managing-tasks.md) covers add, watch, inspect, cancel, and remove operations.
- [Examples](../examples/README.md) provides complete runnable programs for the main Taskvisor workflows.
