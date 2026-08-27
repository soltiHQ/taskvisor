---
title: Production boundaries
description: Understand Taskvisor durability, cancellation, observability, scheduling, and ownership boundaries before deployment.
---

# Production boundaries

## In-process state

- Runtime state, [task IDs](https://docs.rs/taskvisor/latest/taskvisor/identity/struct.TaskId.html), controller queues, watched outcomes, and events are not durable.
- Taskvisor does not recover work after process failure.
- A watched outcome belongs to the current caller and process.

## Cooperative cancellation

- Long-running tasks must observe [`TaskContext`](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskContext.html).
- Synchronous task code cannot be interrupted in the middle of a poll.
- [`ForceAborted`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html#variant.ForceAborted) can arrive before the physical attempt returns.
- Attempt timeout drops the future but cannot undo external side effects.

[Cancellation and shutdown](cancellation-and-shutdown.md) explains the separate task, caller, and shutdown deadlines.

## Best-effort observability

- The shared event bus and subscriber queues can drop events.
- Subscriber callbacks already running cannot be interrupted at the drain deadline.
- Use [watched outcomes](outcomes-and-events.md) rather than events for application correctness.
- Use [`ownership_snapshot`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.ownership_snapshot) for current ownership and deferred-cleanup state.
- [`OwnershipCapacityRetired`](https://docs.rs/taskvisor/latest/taskvisor/events/enum.EventKind.html#variant.OwnershipCapacityRetired) reports a transition through best-effort events.
- Capacity can retire during late cleanup, after event delivery closes. The snapshot remains available.

## Scheduling and coordination scope

- [Periodic work](lifecycle-policies.md) uses a delay after completion, not cron or missed-run recovery.
- [Controller coordination](keyed-admission.md) is local to one supervisor.
- A controller slot is not a cancellation key.
- Supervisor-local budgets do not isolate operating-system CPU, memory, or thread limits.

## Taskvisor-owned native threads

Taskvisor can create native operating-system threads in addition to the application's Tokio runtime.
These counts apply to each supervisor separately.

- Deferred cleanup starts on the first valid non-empty ownership reservation. With configured subscribers, startup is attempted during supervisor construction. Without subscribers, it starts on the first task or controller ownership reservation.
- With the default `ownership_capacity = 1024`, deferred cleanup starts three persistent core workers. When cleanup is queued and no worker is idle, it can add temporary elastic workers up to 16 accounted live-or-starting workers. Elastic workers exit after one idle second.
- For a finite ownership capacity `C`, the persistent and maximum cleanup counts are `min(3, C)` and `min(16, C)`. Disabling the ownership limit keeps the cleanup counts at 3 and 16. These worker counts have no direct public configuration setting.
- Subscriber callback workers start with the supervisor runtime. At least one `Shared` subscriber creates one fixed worker for all shared lanes. Each `Dedicated` subscriber creates one additional fixed worker. No subscriber means no callback worker.
- [`Subscribe::execution`](https://docs.rs/taskvisor/latest/taskvisor/subscribers/trait.Subscribe.html) is read once during supervisor construction. [`TracingBridge`](https://docs.rs/taskvisor/latest/taskvisor/subscribers/struct.TracingBridge.html), its `with_reasons` variant, and [`LogWriter`](https://docs.rs/taskvisor/latest/taskvisor/subscribers/struct.LogWriter.html) select `Dedicated`.

A started supervisor with default ownership capacity and one `TracingBridge` as its only subscriber therefore owns four Taskvisor native threads: three cleanup workers and one callback worker.
Cleanup can temporarily expand toward its separate ceiling of 16.
A finite ownership capacity bounds how many subscribers Taskvisor can own, but it is not a separate callback-thread limit.

## Owned user values

The [deferred cleanup domain](../src/core/deferred_drop/mod.rs) destroys retained task and subscriber values on dedicated threads.
Attempt futures are different: their destructors run synchronously inside the [physical attempt](defining-tasks.md#keep-blocking-work-off-tokio).

- Accepted tasks and configured subscribers consume ownership capacity through physical cleanup.
- Blocking destructors for retained task or subscriber values occupy cleanup workers until they return.
- A [`TaskWaiter`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.TaskWaiter.html) can resolve before the retained task object is destroyed.
- A later destructor panic cannot change that outcome.
- A panic while those values are destroyed permanently retires one unit from a finite ownership capacity.
- [`OwnershipSnapshot`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.OwnershipSnapshot.html) shows limits, available units, waiting requests, and queued or running cleanup.
- Reading the snapshot does not start dormant cleanup workers.
- Ownership timeout methods bound only the permit wait. Command queues, controller policy, registry admission, and execution are outside that deadline.
- Removing the ownership limit allows retained user values and cleanup backlog to grow without a count bound.

See [ownership configuration](configuration.md#bound-different-resources) and the [snapshot fields](../src/core/ownership.rs).

### Cleanup without a runtime context

The [attempt reaper](../src/core/registry/scheduler/reaper.rs) retains actor handles and results after force-abort.
If its coordinator is closed, `spawn_or_retain()` tries to spawn detached cleanup on the current Tokio runtime.
If the calling thread has no current Tokio runtime context, it intentionally leaks the reaping future with [`std::mem::forget`](https://doc.rust-lang.org/std/mem/fn.forget.html).
This avoids dropping its owned values on that thread.

User values and ownership capacity held by the unfinished reaper record remain retained for the rest of the process lifetime.
The capacity stays occupied; this path does not retire it as a destructor panic would.
A runtime may still exist elsewhere in the process. Its destruction is not a precondition for this fallback.

## Check before deployment

- Use durable external state when work must recover after process failure.
- Review every timeout, cancellation, and force-abort boundary for external side effects.
- Make retryable operations safe to repeat or protect them with an application-owned idempotency or acknowledgement protocol.
- Use watched outcomes when application correctness depends on a final task result.
- Choose retry, concurrency, registration, ownership, command-queue, subscriber-queue, and controller limits deliberately.
- Account for Taskvisor-owned native threads under container CPU and process-thread limits.
- Decide whether the application or Taskvisor owns operating-system signal handling.
- Join shutdown from a caller that can report cleanup errors.
- Test cooperative cancellation, grace expiry, and application shutdown behavior.
- Monitor event-loss diagnostics and runtime snapshots where operational visibility requires them.

The [crate root](../src/lib.rs) forbids unsafe Rust with `#![forbid(unsafe_code)]`.
