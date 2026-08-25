---
title: Production boundaries
description: Understand Taskvisor durability, cancellation, observability, scheduling, and ownership boundaries before deployment.
---

# Production boundaries

## In-process state

- Runtime state, task IDs, controller queues, watched outcomes, and events are not durable.
- Taskvisor does not recover work after process failure.
- A watched outcome belongs to the current caller and process.

## Cooperative cancellation

- Long-running tasks must observe `TaskContext`.
- Synchronous task code cannot be interrupted in the middle of a poll.
- `ForceAborted` can be delivered before the physical attempt returns.
- Attempt timeout drops the future but cannot undo external side effects.

## Best-effort observability

- The shared event bus and subscriber queues can drop events.
- Subscriber callbacks already running cannot be interrupted at the drain deadline.
- Use watched outcomes rather than events for application correctness.
- Use `ownership_snapshot` for current ownership and deferred-cleanup state. `OwnershipCapacityRetired` is a best-effort transition diagnostic.
- Retirement can happen after event delivery has closed during late shutdown cleanup. The snapshot remains the current-state interface.

## Scheduling and coordination scope

- Periodic work uses a delay after completion, not cron or missed-run recovery.
- Controller coordination is local to one supervisor.
- A controller slot is not a cancellation key.
- Supervisor-local budgets do not isolate operating-system CPU, memory, or thread limits.

## Owned user values

- Accepted tasks and configured subscribers consume ownership capacity through physical cleanup.
- Blocking destructors for retained task or subscriber values occupy cleanup workers until they return.
- `TaskWaiter` can resolve before final retained-task destruction; a later destructor panic cannot revise that outcome.
- A panic while those values are destroyed permanently retires one unit from a finite ownership capacity.
- `ownership_snapshot` exposes configured, effective, available, waiting, and queued or running cleanup state without starting dormant workers.
- Removing the ownership limit allows retained user values and cleanup backlog to grow without a count bound.

The crate forbids unsafe Rust with `#![forbid(unsafe_code)]`.
