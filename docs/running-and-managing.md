---
title: Run and manage Taskvisor
description: Choose a supervisor entry point and manage registered or controller-submitted work at runtime.
---

# Choose how the supervisor runs

Choose an entry point based on how tasks are supplied and who requests shutdown:

| Entry point                       | Use it when                                                 |
|-----------------------------------|-------------------------------------------------------------|
| `Supervisor::run`                 | The initial batch finishes naturally.                       |
| `Supervisor::run_until`           | The application owns the future that requests shutdown.     |
| `Supervisor::run_with_os_signals` | Taskvisor should install process signal handlers.           |
| `Supervisor::serve`               | Work is discovered or managed while the service is running. |

`run`, `run_until`, and `run_with_os_signals` submit one initial batch through all-or-nothing registry admission.
Admission can reject the full batch.
`run_until` can begin shutdown before the batch commits, and `run_with_os_signals` can enter cleanup before the commit if signal-listener setup fails.
An `Ok(())` return confirms that the shared supervisor lifecycle and cleanup workflow completed; it does not mean every task succeeded.
Use watched work when application logic needs each final result.

Tasks already registered through `serve` keep the registry non-empty and participate in the static lifecycle.
A batch rejected by the registry after the static lifecycle commits consumes that lifecycle; errors before the commit leave it available for another static run.
Registry rejection does not stop tasks that were added earlier through `serve`.
Dropping a static run future after its lifecycle commits does not stop admitted tasks or start shutdown.
A handle returned by `serve` can still request shutdown.

These three methods share one static lifecycle.
After one commits, another static run on the same supervisor returns `RuntimeError::AlreadyRunning`.

`run` and `run_until` do not install operating-system signal handlers.
`run_with_os_signals` is the explicit process-wide opt-in.
An embedded application that already owns signals should use `run_until` or request shutdown through a dynamic handle.

On Unix, dropping Taskvisor's signal listeners does not restore the default signal disposition.
The application remains responsible for signal handling after the method returns.

`serve` starts the same runtime without a static batch and returns a `SupervisorHandle`.
It does not install signal handlers.
Call `handle.shutdown().await` when the application wants the joined cleanup result.

Create a supervisor with `Supervisor::new` when runtime configuration and subscribers are enough.
Use `Supervisor::builder` when the application also needs task defaults, a controller, or typed construction errors through `try_build`.

Runnable entry-point examples:

- [basic.rs](../examples/basic.rs) uses `run`;
- [application_shutdown.rs](../examples/application_shutdown.rs) uses `run_until`;
- [graceful_worker.rs](../examples/graceful_worker.rs) uses `run_with_os_signals`;
- [dynamic_tasks.rs](../examples/dynamic_tasks.rs) uses `serve`.

## Manage tasks at runtime

A dynamic handle separates task registration from task completion.

| Operation          | What an `Ok` result means                                                                                                                            |
|--------------------|------------------------------------------------------------------------------------------------------------------------------------------------------|
| `add`              | The runtime registry accepted the task. The first attempt may not have started yet.                                                                  |
| `add_and_watch`    | Registration succeeded and the caller received a final-outcome waiter.                                                                               |
| `submit`           | The controller accepted the command. Slot admission happens later.                                                                                   |
| `submit_and_watch` | Command intake succeeded and the caller received a waiter for rejection or the admitted task's final outcome.                                        |
| `TaskWaiter::wait` | A direct final in-process outcome was delivered.                                                                                                     |
| `remove`           | The boolean says whether this call created the stop claim. Registered cleanup may continue; queued work is removed before return.                    |
| `cancel`           | The boolean says whether this call created the stop claim. For registered work, registry membership and the final outcome are settled before return. |

`false` can mean the work was unknown, already finished, or already claimed by another stop request.
A `cancel` call that joins an existing removal waits for the same cleanup and also returns `false`.

The `submit*` methods require the `controller` Cargo feature and a supervisor built with a controller.
Without an installed controller, they return `ControllerError::NotConfigured`.

Use `TaskId` for one exact registration or controller submission.
Use `remove_by_name` and `cancel_by_name` for registered work addressed by task name.
Controller work that is still queued does not own a registered task name; stop it with the task ID returned by `submit*`.

`list` returns registry membership.
It includes tasks waiting for attempt capacity, in retry backoff, running, or completing cleanup.
`alive_snapshot` and `is_alive` answer a different question: whether a physical attempt is still active.
Both are point-in-time snapshots and may be stale as soon as concurrent work changes.

Regular `add*` calls wait for ownership admission and registry-command capacity.
Their `try_add*` forms fail fast at both boundaries, then still wait for the registry decision.
Controller `submit*` calls wait for ownership admission and controller-command capacity; their `try_submit*` forms fail fast at both boundaries and return after command intake.
Regular stop operations wait for the required management intake resources, while their `try_*` forms fail fast at those boundaries.
Later registry or controller decisions can still reject work after command intake.
The exact boundary and error are documented on each method in the [API reference](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html).

See [dynamic_tasks.rs](../examples/dynamic_tasks.rs) for one complete management flow.
