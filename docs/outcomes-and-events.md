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

## Separate API errors from task outcomes

An API error means that the current call did not return its documented success value.
It does not by itself prove that no command or state transition was committed; the contract of the specific method defines that boundary.
A `TaskOutcome` reports how watched work finally ended.

```text
add_and_watch(spec).await
├── Err(RuntimeError)       no watched registration was returned
└── Ok((_id, waiter))
         └── waiter.wait().await
              ├── Err(RuntimeError::OutcomeUnavailable)  result channel failed
              └── Ok(TaskOutcome)                        work reached a final state
```

| Boundary                                                              | Type              |
|-----------------------------------------------------------------------|-------------------|
| Checked configuration constructors and setters                        | `ConfigError`     |
| `BackoffPolicy::new`                                                  | `BackoffError`    |
| `SupervisorBuilder::try_build`                                        | `BuildError`      |
| Runtime lifecycle, management, wait, and shutdown                     | `RuntimeError`    |
| Controller preparation and command intake                             | `ControllerError` |
| One task attempt                                                      | `TaskError`       |
| Final result of watched work                                          | `TaskOutcome`     |
| Application code that combines runtime and controller calls           | `Error`           |

`SupervisorBuilder::build` and `Supervisor::new` panic when checked construction would fail.
Use `try_build` when the application must report or recover from construction failure.

`Ok(TaskOutcome::Failed { .. })` means outcome delivery succeeded and the task ended in failure.
It is not an API error.
For controller work, `submit_and_watch().await?` confirms command intake; the waiter can later deliver `TaskOutcome::Rejected` after slot or registry admission fails.

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

## Branch on a final outcome

Match typed variants for application decisions and use stable labels for telemetry.
Keep a fallback arm because `TaskOutcome` is non-exhaustive.

```rust
use taskvisor::{TaskOutcome, TaskWaiter};

async fn report(waiter: TaskWaiter) -> Result<(), Box<dyn std::error::Error>> {
    let outcome = waiter.wait().await?;

    match &outcome {
        TaskOutcome::Completed => println!("completed"),
        TaskOutcome::Failed { reason, .. } => println!("failed: {reason}"),
        TaskOutcome::Fatal { reason, .. } => println!("fatal: {reason}"),
        TaskOutcome::Rejected { kind, .. } => println!("rejected: {kind:?}"),
        other => println!("ended: {}", other.as_label()),
    }

    Ok(())
}
```

Reason strings are diagnostic text.
Do not parse them as a classification API.

## Understand ForceAborted

`ForceAborted` normally follows the configured grace period.
Last-owner fallback and signal-setup failure cleanup cannot wait for that period.
The physical attempt can remain active until synchronous task code returns control to Tokio.

## Treat events as observability

Choose the interface that answers the operational question:

| Need                                  | Interface                                      |
|---------------------------------------|------------------------------------------------|
| Application decision                  | `TaskWaiter` and `TaskOutcome`                 |
| Readable demo or small-tool logs      | `LogWriter` with the `logging` feature         |
| Structured service telemetry          | `TracingBridge` with the `tracing` feature     |
| Application-owned metrics             | A custom `Subscribe` implementation            |
| Registry membership                   | `list`                                         |
| Physical attempt activity             | `alive_snapshot`                               |
| Retained values and cleanup pressure  | `ownership_snapshot`                           |
| Per-key admission state               | `controller_snapshot`                          |

The shared event bus and every subscriber queue are bounded.
Events can be lost at the shared bus or in an individual subscriber queue.
When the shared bus is full, it drops the oldest event and retains the newest one.
When one subscriber queue is full, Taskvisor drops the incoming event for that subscriber.
Each subscriber receives callbacks serially in its own order, but two different subscribers can run at the same time.

Subscriber callbacks are synchronous and run outside Tokio worker threads. Keep them short.
Forward async or long blocking work to an application-owned queue.
Overflow and shutdown deadlines can drop events; overflow diagnostics report loss when possible.

See [outcomes.rs](../examples/outcomes.rs), [custom_subscriber.rs](../examples/custom_subscriber.rs), [logging.rs](../examples/logging.rs) (requires `logging`), [tracing.rs](../examples/tracing.rs) (requires `tracing`), and [metrics.rs](../examples/metrics.rs).
