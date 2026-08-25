---
title: Manage tasks at runtime
description: Add, watch, inspect, cancel, and remove registered or controller-submitted work through SupervisorHandle.
---

# Manage tasks at runtime

A dynamic handle separates task registration from task completion.

## Choose an operation

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

## Address registered and queued work

Use `TaskId` for one exact registration or controller submission.
Use `remove_by_name` and `cancel_by_name` for registered work addressed by task name.
Controller work that is still queued does not own a registered task name; stop it with the task ID returned by `submit*`.

## Inspect runtime state

`list` returns registry membership.
It includes tasks waiting for attempt capacity, in retry backoff, running, or completing cleanup.
`alive_snapshot` and `is_alive` answer a different question: whether a physical attempt is still active.
`ownership_snapshot` reports the separate lifetime and deferred-cleanup boundary.
It can still show in-use ownership or deferred cleanup after a completed task disappears from `list` and `alive_snapshot`.
These views are point-in-time diagnostics and can become stale immediately.
See [Configure Taskvisor](configuration.md#observe-ownership-pressure) for the ownership fields and a reporting example.

## Choose waiting, bounded ownership, or fail-fast intake

Direct registry work has these intake boundaries:

| Method family                         | Ownership admission | Registry command queue | Registry decision |
|---------------------------------------|---------------------|------------------------|-------------------|
| `add*`                                | Waits.              | Waits.                 | Waits.            |
| `add*_with_ownership_timeout`         | Caller deadline.    | Waits without it.      | Waits without it. |
| `try_add*`                            | Fails fast.         | Fails fast.            | Waits.            |

Controller submissions have different completion semantics:

| Method family                         | Ownership admission | Controller command queue | Slot and registry admission |
|---------------------------------------|---------------------|--------------------------|-----------------------------|
| `submit*`                             | Waits.              | Waits.                   | Happens later.              |
| `submit*_with_ownership_timeout`      | Caller deadline.    | Waits without it.        | Happens later.              |
| `try_submit*`                         | Fails fast.         | Fails fast.              | Happens later.              |

An ownership timeout is not an end-to-end task deadline.
It stops after Taskvisor acquires the ownership permit.
`try_add*` still waits for the registry decision after successful intake, while `try_submit*` returns after controller command intake.
Once a state-changing method commits its command, dropping the caller's future does not cancel that command.
Dropping a returned `TaskWaiter` also does not cancel the task or submission.
Keep the returned task ID when the surrounding request can end before the work and the application may need an explicit stop operation.

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
Taskvisor does not map these failures to a transport-specific status.

An ownership timeout sends no command, starts no task, and publishes no lifecycle event for the request.
A zero duration still accepts an ownership permit that is immediately ready. The timer cannot interrupt synchronous
startup of dormant cleanup workers.

Regular stop operations wait for the required management intake resources, while their `try_*` forms fail fast at those boundaries.
Later registry or controller decisions can still reject work after command intake.
The exact boundary and error are documented on each method in the [API reference](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html).

## Run the management example

See [dynamic_tasks.rs](../examples/dynamic_tasks.rs) for one complete management flow.
