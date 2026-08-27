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

### Choose the application Tokio runtime

Taskvisor uses the active application runtime; it does not choose a Tokio runtime flavor for the application.
Choose from the workload and deployment constraints, not from task duration alone:

| Application boundary                                                                                          | Starting point               |
|---------------------------------------------------------------------------------------------------------------|------------------------------|
| The process intentionally uses one Tokio worker, and measured management concurrency and task latency fit    | `current_thread`             |
| Multiple independent futures need to be runnable on Tokio workers concurrently                               | `multi_thread`               |
| Task code performs heavy CPU work or blocking calls                                                           | Neither runtime flavor fixes the task; use a suitable CPU or blocking pool and await its result from the supervised task |
| The production workload is mixed or its ready-future pattern is not yet known                                 | Measure the real workload on both runtime flavors |

For example, an intentionally single-threaded CLI with non-blocking timers or queue receive loops can start with:

```rust
#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Construct and run Taskvisor here.
}
```

A service that needs several Tokio workers to run independent ready futures can start with:

```rust
#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    // Construct and run Taskvisor here.
}
```

Four workers match the benchmark fixture below; they are not a general worker-count recommendation.
Taskvisor can also own native cleanup and subscriber threads, so `current_thread` describes Tokio only.

Two matched benchmark boundaries show why task duration alone is not a runtime-selection rule:

- **Root-driven serialized lifecycle.**
  For 256 instant watched tasks, from the first serialized root-caller admission through all outcomes, the Taskvisor 0.9.0 Linux/aarch64 reference run measured 95.661 K tasks/s on current-thread and 55.467 K tasks/s on multi-thread with four workers.
  Current-thread was faster for this boundary.
- **Pre-admitted cooperative CPU drain.**
  For 64 already-admitted tasks, each running 16 independent CPU chunks of 4,096 steps and yielding after every chunk, the same run measured 12.446 K tasks/s on current-thread and 29.945 K tasks/s on multi-thread with four workers.
  Multi-thread was `2.406×` faster for this boundary.

The first result does not establish that short tasks are generally faster on current-thread.
It includes serialized caller-to-registry round trips and contains no parallel task work.
The second result excludes admission and includes user CPU work, so `2.406×` is the speedup of that complete synthetic drain boundary, not a measurement of Taskvisor overhead alone.
See the [throughput](../benches/README.md#throughput) and [dynamic-management](../benches/README.md#dynamic-management) benchmark contracts before comparing results.
Keep heavy CPU work [off Tokio](defining-tasks.md#keep-blocking-work-off-tokio); [cpu_job.rs](../examples/cpu_job.rs) shows a supervised bridge to Rayon.

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
