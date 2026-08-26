---
title: Manage tasks at runtime
description: Add, watch, inspect, cancel, and remove registered or controller-submitted work through SupervisorHandle.
---

# Manage tasks at runtime

A [SupervisorHandle](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html) lets you manage tasks while the service is running.
The registry tracks accepted tasks by ID and name. Registration does not mean that the task has started or finished.

## Choose an operation

| Operation                                                                                                                  | What an `Ok` result means                                                                                                     |
|----------------------------------------------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------------------------|
| [`add`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.add)                           | The runtime registry accepted the task. The first attempt may not have started yet.                                           |
| [`add_and_watch`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.add_and_watch)       | Registration succeeded and the caller received a final-outcome waiter.                                                        |
| [`submit`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.submit)                     | The controller accepted the command. The return is not an admission decision.                                                 |
| [`submit_and_watch`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.submit_and_watch) | Command intake succeeded and the caller received a waiter for rejection or the admitted task's final outcome.                 |
| [`TaskWaiter::wait`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.TaskWaiter.html#method.wait)                   | A direct final in-process outcome was delivered.                                                                              |
| [`remove`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.remove)                     | `true` means this call started removal. Registered cleanup may continue; queued work is removed before return.                |
| [`cancel`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.cancel)                     | `true` means this call started removal. Registered work has left the registry and its final outcome is settled before return. |

`false` can mean the work was unknown, already finished, or already claimed by another stop request.
A [`cancel`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.cancel) call that joins an existing removal waits for the same cleanup and also returns `false`.
After a force-abort, physical task code may still be running even after [`cancel`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.cancel) returns.
See [Cancellation and shutdown](cancellation-and-shutdown.md) for grace periods and caller deadlines.

The `submit*` methods require the `controller` Cargo feature and a supervisor built with a controller.
Without an installed controller, they return [`ControllerError::NotConfigured`](https://docs.rs/taskvisor/latest/taskvisor/controller/enum.ControllerError.html#variant.NotConfigured).

## Address registered and queued work

Use [TaskId](https://docs.rs/taskvisor/latest/taskvisor/identity/struct.TaskId.html) for one exact registration or controller submission.
Use [`remove_by_name`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.remove_by_name) and [`cancel_by_name`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.cancel_by_name) for registered work addressed by task name.
Controller work that is still queued does not own a registered task name; stop it with the task ID returned by `submit*`.

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

| Method family                         | Ownership admission | Registry command queue | Registry decision |
|---------------------------------------|---------------------|------------------------|-------------------|
| `add*`                                | Waits.              | Waits.                 | Waits.            |
| `add*_with_ownership_timeout`         | Caller deadline.    | Waits without it.      | Waits without it. |
| `try_add*`                            | Fails fast.         | Fails fast.            | Waits.            |

Controller submissions have different completion semantics:

| Method family                    | Ownership admission | Controller command queue | Slot and registry admission   |
|----------------------------------|---------------------|--------------------------|-------------------------------|
| `submit*`                        | Waits.              | Waits.                   | Does not wait for a decision. |
| `submit*_with_ownership_timeout` | Caller deadline.    | Waits without it.        | Does not wait for a decision. |
| `try_submit*`                    | Fails fast.         | Fails fast.              | Does not wait for a decision. |

The controller can process a submitted command before or after the call returns.

An ownership timeout does not limit the whole task.
It ends as soon as Taskvisor gets the ownership permit.
`try_add*` still waits for the registry decision after successful intake, while `try_submit*` returns after controller command intake.
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
    match handle.try_add_and_watch(spec).await {
        Ok((_id, waiter)) => Ok(Some(waiter)),
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

Regular stop operations wait for the required management intake resources, while their `try_*` forms fail fast at those boundaries.
Later registry or controller decisions can still reject work after command intake.
The exact boundary and error are documented on each method in the [API reference](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html).

## Run the management example

See [dynamic_tasks.rs](../examples/dynamic_tasks.rs) for one complete management flow.

Source: [handle methods](../src/core/handle.rs), [command routing](../src/core/runtime/management/mod.rs), and [registry stop decisions](../src/core/registry/removal/commands.rs).
