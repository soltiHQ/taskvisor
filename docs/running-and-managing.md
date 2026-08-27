---
title: Run Taskvisor
description: Choose a supervisor entry point, understand its static lifecycle, and decide who owns shutdown.
---

# Run Taskvisor

## Choose an entry point

Choose an entry point based on how tasks are supplied and who requests shutdown:

| Entry point                                                                                                                            | Use it when                                                 |
|----------------------------------------------------------------------------------------------------------------------------------------|-------------------------------------------------------------|
| [`Supervisor::run`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.run)                                 | The initial batch finishes naturally.                       |
| [`Supervisor::run_until`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.run_until)                     | The application owns the future that requests shutdown.     |
| [`Supervisor::run_with_os_signals`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.run_with_os_signals) | Taskvisor should install process signal handlers.           |
| [`Supervisor::serve`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.serve)                             | Work is discovered or managed while the service is running. |

## Construct and start one runtime

Use [`Supervisor::new`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.new) for runtime configuration and subscribers.
Use [`Supervisor::builder`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.builder) for task defaults or a controller.
The builder's [`try_build`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorBuilder.html#method.try_build) returns typed construction errors.
Panics from subscriber metadata methods ([`execution`](https://docs.rs/taskvisor/latest/taskvisor/subscribers/trait.Subscribe.html), [`name`](https://docs.rs/taskvisor/latest/taskvisor/subscribers/trait.Subscribe.html#method.name), or [`queue_capacity`](https://docs.rs/taskvisor/latest/taskvisor/subscribers/trait.Subscribe.html#method.queue_capacity)) still reach the caller.

Construction does not start Tokio tasks.
[`serve`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.serve) or a static run starts the runtime. First startup needs an active Tokio runtime.
Repeated [`serve`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.serve) calls return handles to the same runtime; workers start only once.
Shutdown is terminal. Calling [`serve`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.serve) again does not restart workers or reopen admission.
See the [supervisor entry points](../src/core/supervisor.rs) and [startup code](../src/core/runtime/lifecycle/mod.rs).

## Run resident work under application-owned shutdown

This flow starts one resident worker and uses an application-owned future to request shutdown.
It joins the shared shutdown workflow before returning.
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

After the initial batch is admitted, [`run_until`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.run_until) waits for any of these conditions:

- the registry becomes empty;
- the application shutdown future resolves;
- another caller starts shared shutdown.

It then joins shared shutdown. When the application future wins, shutdown requests cooperative task cancellation.
The method can return before the application future resolves if all registered tasks finish first.
The [static-run code](../src/core/runtime/lifecycle/static_run.rs) owns these races.

`Ok(())` reports completion of the supervisor lifecycle, not the worker's outcome.
The wrapped Tokio sleep is safe to stop by dropping.
A real receive, commit, or acknowledgement operation needs its own [cancellation-safety review](cancellation-and-shutdown.md#make-operations-cancellation-aware).

Use [`serve`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.serve) with `add(spec).watch().execute()` when work arrives after startup or application logic needs each task's final outcome.

## Understand the static lifecycle

[`run`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.run), [`run_until`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.run_until), and [`run_with_os_signals`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.run_with_os_signals) submit one initial batch through [all-or-nothing registry admission](../src/core/registry/admission/batch.rs).
The registry accepts every task in that batch or rejects the full batch.
After successful admission, all three methods start natural shutdown when the entire registry becomes empty.
Tasks already added through [`serve`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.serve) also keep it non-empty.

The three methods share one static lifecycle. Its commit is separate from registry admission.
After a call commits, another static run on the same supervisor returns [`RuntimeError::AlreadyRunning`](https://docs.rs/taskvisor/latest/taskvisor/error/enum.RuntimeError.html#variant.AlreadyRunning).
Errors before the lifecycle commit leave it available for another static run.
A registry rejection consumes the lifecycle, but does not stop tasks added earlier through [`serve`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.serve).

[`run_until`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.run_until) can begin shutdown before the batch reaches the registry.
[`run_with_os_signals`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.run_with_os_signals) can enter cleanup before that point if signal-listener setup fails.
An `Ok(())` return means the bounded supervisor lifecycle and shared cleanup workflow completed.
It does not mean every task succeeded or every retained user value has been destroyed.

Dropping a static run future after its lifecycle commits does not stop admitted tasks or start shutdown.
A handle returned by [`serve`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.serve) can still [request shutdown](cancellation-and-shutdown.md#join-shutdown).

## Own signal handling and shutdown

[`run`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.run) and [`run_until`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.run_until) do not install operating-system signal handlers.
[`run_with_os_signals`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.run_with_os_signals) is the explicit process-wide opt-in.
An embedded application that already owns signals should use [`run_until`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.run_until) or request shutdown through a dynamic handle.

On Unix, dropping Taskvisor's signal listeners does not restore the default signal disposition.
The application remains responsible for signal handling after the method returns.

[`serve`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.serve) does not install signal handlers.
Call [`handle.shutdown().await`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.shutdown) for the shared shutdown result.
Read [Cancellation and shutdown](cancellation-and-shutdown.md) before relying on its deadlines or return value.

## Run an entry-point example

Runnable entry-point examples:

- [basic.rs](../examples/basic.rs) uses [`run`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.run);
- [application_shutdown.rs](../examples/application_shutdown.rs) uses [`run_until`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.run_until);
- [graceful_worker.rs](../examples/graceful_worker.rs) uses [`run_with_os_signals`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.run_with_os_signals);
- [dynamic_tasks.rs](../examples/dynamic_tasks.rs) uses [`serve`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.serve).
