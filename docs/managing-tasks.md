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
Both are point-in-time snapshots and may be stale as soon as concurrent work changes.

## Choose waiting or fail-fast intake

Regular `add*` calls wait for ownership admission and registry-command capacity.
Their `try_add*` forms fail fast at both boundaries, then still wait for the registry decision.
Controller `submit*` calls wait for ownership admission and controller-command capacity; their `try_submit*` forms fail fast at both boundaries and return after command intake.
Regular stop operations wait for the required management intake resources, while their `try_*` forms fail fast at those boundaries.
Later registry or controller decisions can still reject work after command intake.
The exact boundary and error are documented on each method in the [API reference](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html).

## Run the management example

See [dynamic_tasks.rs](../examples/dynamic_tasks.rs) for one complete management flow.
