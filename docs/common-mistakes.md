---
title: Common mistakes
description: Avoid incorrect assumptions about task results, admission, cancellation, blocking work, and side effects.
---

# Common mistakes

## Do not treat run success as task success

An `Ok(())` result from [`Supervisor::run`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.run) means the supervisor lifecycle and cleanup workflow completed.
It does not report each task's final outcome.
When application logic needs that outcome, use [`serve`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.serve), [`add_and_watch`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.add_and_watch), [`TaskWaiter::wait`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.TaskWaiter.html#method.wait), and then [`shutdown`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.shutdown).
See [Run Taskvisor](running-and-managing.md) and [Final outcomes and lifecycle events](outcomes-and-events.md).

## Do not treat submit success as admission

[`submit().await?`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.submit) means that the controller accepted the command.
It does not confirm slot admission or runtime registration.
The controller may process the command before or after the call returns.
Use [`submit_and_watch`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.submit_and_watch) and await its [`TaskWaiter`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.TaskWaiter.html) when the application must distinguish rejection from an admitted task outcome.
See [Coordinate work by key](keyed-admission.md).

## Do not drive application logic from events

The event bus and subscriber queues are bounded and best-effort.
Use [`TaskWaiter`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.TaskWaiter.html) for the direct in-process final-outcome path.
Reserve events for logs, metrics, tracing, and live diagnostics.
See [Final outcomes and lifecycle events](outcomes-and-events.md).

## Make resident work observe cancellation

Await [`TaskContext::cancelled`](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskContext.html#method.cancelled), check [`is_cancelled`](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskContext.html#method.is_cancelled) between work units, or wrap a drop-safe future with [`run_until_cancelled`](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskContext.html#method.run_until_cancelled).
Return [`TaskError::Canceled`](https://docs.rs/taskvisor/latest/taskvisor/error/enum.TaskError.html#variant.Canceled) after cooperative cleanup.
See [Cancellation and shutdown](cancellation-and-shutdown.md).

## Do not use a controller slot as task identity

A slot coordinates admission.
It is not a registered task name or a cancellation key.
Use the returned [`TaskId`](https://docs.rs/taskvisor/latest/taskvisor/identity/struct.TaskId.html) for queued work.
The same ID continues to identify the task after registry admission, and registered work can also be addressed by name.
Taskvisor has no slot-wide cancel operation.
See [Coordinate work by key](keyed-admission.md).

## Move blocking and CPU-heavy work off Tokio

Use a separate blocking executor, worker pool, or external runtime.
Keep attempt-future destructors short.
The CPU example uses Rayon and shows that cancellation drops the receiver but does not stop computation already running.
See [Define a task](defining-tasks.md) and [cpu_job.rs](../examples/cpu_job.rs).

## Do not retry an ambiguous side effect blindly

A retry creates a fresh attempt.
It can repeat an external side effect whose previous result was ambiguous, and Taskvisor does not roll that effect back.
Classify the failure as retryable only when repeating the operation is acceptable.
See [Choose task behavior](lifecycle-policies.md).

## Treat timeouts as stop requests, not rollback

An attempt timeout cancels the context and drops the attempt future.
[`ForceAborted`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html#variant.ForceAborted) can become final before physical execution exits.
Neither action undoes external side effects.
Check cancellation safety and use an explicit commit or acknowledgement protocol when dropping an operation is unsafe.
See [Cancellation and shutdown](cancellation-and-shutdown.md) and [Production boundaries](production-boundaries.md).

## Do not treat an ownership timeout as a task deadline

Ownership-specific timeout methods bound only the wait for cleanup ownership before command intake.
They do not bound later command queues, controller admission, registry admission, task execution, or final outcome delivery.
See [Manage tasks at runtime](managing-tasks.md) and [Coordinate work by key](keyed-admission.md).

## Continue learning

| Resource                                        | Next step                                          |
|-------------------------------------------------|----------------------------------------------------|
| [Examples guide](../examples/README.md)         | Choose a complete runnable scenario.               |
| [API documentation](https://docs.rs/taskvisor)  | Read exact contracts for public types and methods. |
| [crates.io](https://crates.io/crates/taskvisor) | Find published versions and package details.       |
| [Benchmark guide](../benches/README.md)         | Run and interpret the Criterion suites.            |
| [Contributor map](../src/ARCHITECTURE.md)       | Follow runtime ownership and source boundaries.    |
