---
title: Final outcomes and lifecycle events
description: Use reliable in-process final outcomes for decisions and best-effort events for observability.
---

# Final outcomes and lifecycle events

## Choose the result path

Taskvisor has two result paths with different contracts:

| Path                           | Contract                                        | Use it for                              |
|--------------------------------|-------------------------------------------------|-----------------------------------------|
| `TaskWaiter` and `TaskOutcome` | One direct final result, outside the event bus. | Application decisions.                  |
| `Subscribe` and `Event`        | Best-effort bounded delivery.                   | Logs, metrics, traces, and live status. |

A watched outcome is independent of event loss while the process and runtime remain alive.
It is not durable storage.
`TaskWaiter::wait` can return `OutcomeUnavailable` if its completion channel closes unexpectedly.

## Handle final outcomes

Final outcomes distinguish:

| Outcome        | Meaning                                                                                 |
|----------------|-----------------------------------------------------------------------------------------|
| `Completed`    | The final attempt succeeded and the restart policy stopped the task.                    |
| `Failed`       | Retryable failure stopped under policy or retry limit.                                  |
| `Fatal`        | The task reported a permanent failure.                                                  |
| `Canceled`     | Cancellation was requested or reported.                                                 |
| `ForceAborted` | Taskvisor stopped waiting before cooperative termination completed.                     |
| `Panicked`     | The actor or protected attempt-owned cleanup panicked before terminal outcome delivery. |
| `Rejected`     | Admission rejected the work, or queued controller work was removed.                     |

Use stable outcome and rejection kinds for branching, metrics, and alerts.
Treat reason strings as diagnostic text.
A panic while polling task code becomes a retryable task failure instead of `Panicked`.
Removing watched controller work before it runs produces `Rejected` with `RejectionKind::RemovedFromQueue`, not `Canceled`.

Taskvisor delivers the terminal outcome before deferred cleanup destroys the retained task object and physical result.
That later destruction can block or panic, but it cannot revise an outcome already delivered through `TaskWaiter`.
Destructor failures on that later path are runtime diagnostics rather than `TaskOutcome::Panicked`.

## Understand ForceAborted

`ForceAborted` normally follows the configured grace period.
Last-owner fallback and signal-setup failure cleanup cannot wait for that period.
The physical attempt can remain active until synchronous task code returns control to Tokio.

## Treat events as observability

The shared event bus and every subscriber queue are bounded.
Events can be lost at the shared bus or in an individual subscriber queue.
When the shared bus is full, it drops the oldest event and retains the newest one.
When one subscriber queue is full, Taskvisor drops the incoming event for that subscriber.
Each subscriber receives callbacks serially in its own order, but two different subscribers can run at the same time.

Subscriber callbacks are synchronous and run outside Tokio worker threads. Keep them short.
Forward async or long blocking work to an application-owned queue.
Overflow and shutdown deadlines can drop events; overflow diagnostics report loss when possible.

See [outcomes.rs](../examples/outcomes.rs), [custom_subscriber.rs](../examples/custom_subscriber.rs), [logging.rs](../examples/logging.rs) (requires `logging`), [tracing.rs](../examples/tracing.rs) (requires `tracing`), and [metrics.rs](../examples/metrics.rs).
