---
title: Run Taskvisor
description: Choose a supervisor entry point, understand its static lifecycle, and decide who owns shutdown.
---

# Run Taskvisor

## Choose an entry point

Choose an entry point based on how tasks are supplied and who requests shutdown:

| Entry point                       | Use it when                                                 |
|-----------------------------------|-------------------------------------------------------------|
| `Supervisor::run`                 | The initial batch finishes naturally.                       |
| `Supervisor::run_until`           | The application owns the future that requests shutdown.     |
| `Supervisor::run_with_os_signals` | Taskvisor should install process signal handlers.           |
| `Supervisor::serve`               | Work is discovered or managed while the service is running. |

## Run resident work under application-owned shutdown

This complete flow starts one resident worker, uses an application-owned future to request shutdown, and joins cleanup before returning.
Replace the timer with the surrounding server's shutdown future.

```rust
use std::time::Duration;
use taskvisor::prelude::*;

async fn application_shutdown() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let worker: TaskRef = TaskFn::arc(|ctx| async move {
        loop {
            ctx.run_until_cancelled(tokio::time::sleep(Duration::from_secs(1)))
                .await?;
            // Poll or process one cancellation-safe unit of work.
        }
    });

    let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    supervisor
        .run_until(
            vec![TaskSpec::restartable("worker", worker)],
            application_shutdown(),
        )
        .await?;

    Ok(())
}
```

`run_until` admits the initial batch and supervises it until the application future resolves.
It then requests cooperative cancellation and joins the shared cleanup workflow.
Its `Ok(())` result reports completion of the supervisor lifecycle, not the worker's individual outcome.
The wrapped Tokio sleep is safe to stop by dropping. A real receive, commit, or acknowledgement operation needs its own cancellation-safety review.

Use `serve` with watched `add*` methods instead when work arrives after startup or application logic needs each final task outcome.

`run`, `run_until`, and `run_with_os_signals` submit one initial batch through all-or-nothing registry admission.
Admission can reject the full batch.
`run_until` can begin shutdown before the batch commits, and `run_with_os_signals` can enter cleanup before the commit if signal-listener setup fails.
An `Ok(())` return confirms that the shared supervisor lifecycle and cleanup workflow completed; it does not mean every task succeeded.
Use watched work when application logic needs each final result.

## Understand the static lifecycle

Tasks already registered through `serve` keep the registry non-empty and participate in the static lifecycle.
A batch rejected by the registry after the static lifecycle commits consumes that lifecycle; errors before the commit leave it available for another static run.
Registry rejection does not stop tasks that were added earlier through `serve`.
Dropping a static run future after its lifecycle commits does not stop admitted tasks or start shutdown.
A handle returned by `serve` can still request shutdown.

These three methods share one static lifecycle.
After one commits, another static run on the same supervisor returns `RuntimeError::AlreadyRunning`.

## Own signal handling and shutdown

`run` and `run_until` do not install operating-system signal handlers.
`run_with_os_signals` is the explicit process-wide opt-in.
An embedded application that already owns signals should use `run_until` or request shutdown through a dynamic handle.

On Unix, dropping Taskvisor's signal listeners does not restore the default signal disposition.
The application remains responsible for signal handling after the method returns.

`serve` starts the same runtime without a static batch and returns a `SupervisorHandle`.
It does not install signal handlers.
Call `handle.shutdown().await` when the application wants the joined cleanup result.

## Choose how to construct the supervisor

Create a supervisor with `Supervisor::new` when runtime configuration and subscribers are enough.
Use `Supervisor::builder` when the application also needs task defaults, a controller, or typed construction errors through `try_build`.

## Run an entry-point example

Runnable entry-point examples:

- [basic.rs](../examples/basic.rs) uses `run`;
- [application_shutdown.rs](../examples/application_shutdown.rs) uses `run_until`;
- [graceful_worker.rs](../examples/graceful_worker.rs) uses `run_with_os_signals`;
- [dynamic_tasks.rs](../examples/dynamic_tasks.rs) uses `serve`.
