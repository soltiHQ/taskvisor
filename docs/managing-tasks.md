---
title: Manage tasks at runtime
description: Add, watch, inspect, cancel, and remove registered or controller-submitted work through SupervisorHandle.
---

# Manage tasks at runtime

A [SupervisorHandle](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html) lets you manage tasks while the service is running.
The registry tracks accepted tasks by ID and name. Registration does not mean that the task has started or finished.

## Choose an operation

| Operation                                                                                                | What an `Ok` result means                                                                                                     |
|----------------------------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------------------------|
| `add(spec).execute()`                                                                                    | The runtime registry accepted the task. The first attempt may not have started yet.                                           |
| `add(spec).watch().execute()`                                                                            | Registration succeeded and the caller received a final-outcome waiter.                                                        |
| `submit(request).execute()`                                                                              | The controller accepted the command. The return is not an admission decision.                                                 |
| `submit(request).watch().execute()`                                                                      | Command intake succeeded and the caller received a waiter for rejection or the admitted task's final outcome.                 |
| [`TaskWaiter::wait`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.TaskWaiter.html#method.wait) | A direct final in-process outcome was delivered.                                                                              |
| `remove(target).execute()`                                                                               | `true` means this call started removal. Registered cleanup may continue; queued work is removed before return.                |
| `cancel(target).execute()`                                                                               | `true` means this call started removal. Registered work has left the registry and its final outcome is settled before return. |

`false` can mean the work was unknown, already finished, or already claimed by another stop request.
A [`cancel`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.cancel) operation whose `execute().await` joins an existing removal waits for the same cleanup and returns `false`.
After a force-abort, physical task code may still be running even after `cancel(target).execute().await` returns.
See [Cancellation and shutdown](cancellation-and-shutdown.md) for grace periods and caller deadlines.

Controller submission operations require the `controller` Cargo feature and a supervisor built with a controller.
Without an installed controller, their terminal methods return [`ControllerError::NotConfigured`](https://docs.rs/taskvisor/latest/taskvisor/controller/enum.ControllerError.html#variant.NotConfigured).

## Address registered and queued work

Use [TaskId](https://docs.rs/taskvisor/latest/taskvisor/identity/struct.TaskId.html) for one exact registration or controller submission.
Pass a task name to [`remove`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.remove) or [`cancel`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.cancel) for registered work addressed by name.
Controller work that is still queued does not own a registered task name; stop it with the task ID returned by `submit(request).execute().await`.

## Inspect runtime state

[list](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.list) returns the tasks still in the registry.
It includes tasks waiting for attempt capacity, in retry backoff, running, or completing cleanup.
[`alive_snapshot`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.alive_snapshot) and [`is_alive`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.is_alive) answer a different question: whether a physical attempt is still active.
[ownership_snapshot](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.ownership_snapshot) reports values still owned by Taskvisor and work left for cleanup workers.
It can still show in-use ownership or deferred cleanup after a completed task disappears from [`list`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.list) and [`alive_snapshot`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.alive_snapshot).
These views describe the state when read. That state can change immediately.
See [Configure Taskvisor](configuration.md#observe-ownership-pressure) for the ownership fields and a reporting example.

## Choose waiting, bounded ownership, or fail-fast intake

Direct registry work has these intake boundaries:

| Operation                                      | Ownership admission | Registry command queue | Registry decision |
|------------------------------------------------|---------------------|------------------------|-------------------|
| `add(spec).execute()`                          | Waits.              | Waits.                 | Waits.            |
| `add(spec).ownership_timeout(d).execute()`     | Caller deadline.    | Waits without it.      | Waits without it. |
| `add(spec).fail_fast().execute()`              | Fails fast.         | Fails fast.            | Waits.            |

Controller submissions have different completion semantics:

| Operation                                             | Ownership admission | Controller command queue | Slot and registry admission   |
|-------------------------------------------------------|---------------------|--------------------------|-------------------------------|
| `submit(request).execute()`                           | Waits.              | Waits.                   | Does not wait for a decision. |
| `submit(request).ownership_timeout(d).execute()`      | Caller deadline.    | Waits without it.        | Does not wait for a decision. |
| `submit(request).try_intake()`                        | Fails fast.         | Fails fast.              | Does not wait for a decision. |

The controller can process a submitted command before or after the call returns.

An ownership timeout does not limit the whole task.
It ends as soon as Taskvisor gets the ownership permit.
`add(spec).fail_fast().execute()` still waits for the registry decision after successful intake, while `submit(request).try_intake()` returns after controller command intake.
Once a state-changing method commits its command, dropping the caller's future does not cancel that command.
Dropping a returned [`TaskWaiter`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.TaskWaiter.html) also does not cancel the task or submission.
Keep the returned task ID if the application may need to stop the work later.

Use fail-fast intake when the application must choose its own overload behavior:

```rust
use taskvisor::{Error, RuntimeError, SupervisorHandle, TaskSpec, TaskWaiter};

async fn try_accept(
    handle: &SupervisorHandle,
    spec: TaskSpec,
) -> Result<Option<TaskWaiter>, Error> {
    match handle.add(spec).watch().fail_fast().execute().await {
        Ok(waiter) => Ok(Some(waiter)),
        Err(
            RuntimeError::ResourceLimitReached { .. }
            | RuntimeError::CommandQueueFull,
        ) => Ok(None), // Apply the application's overload policy.
        Err(error) => Err(error.into()),
    }
}
```

The application decides whether overload means retry, shedding work, or another response.
Taskvisor does not choose an HTTP status or another transport response for these failures.

An ownership timeout sends no command, starts no task, and publishes no lifecycle event for the request.
A zero duration still accepts an ownership permit that is immediately ready. The timer cannot interrupt synchronous
startup of dormant cleanup workers.

Regular stop operations wait for the required management intake resources, while the `fail_fast()` modifier rejects unavailable command capacity immediately.
Later registry or controller decisions can still reject work after command intake.
The exact boundary and error are documented on each method in the [API reference](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html).

## Run the management example

See [dynamic_tasks.rs](../examples/dynamic_tasks.rs) for one complete management flow.

Source: [handle methods](../src/core/handle.rs), [command routing](../src/core/runtime/management/mod.rs), and [registry stop decisions](../src/core/registry/removal/commands.rs).
