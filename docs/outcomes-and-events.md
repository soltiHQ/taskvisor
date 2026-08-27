---
title: Final outcomes and lifecycle events
description: Use reliable in-process final outcomes for decisions and best-effort events for observability.
---

# Final outcomes and lifecycle events

## Choose the result path

Taskvisor has two result paths with different contracts:

| Path                                                                                                                                                                          | Contract                                        | Use it for                              |
|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-------------------------------------------------|-----------------------------------------|
| [TaskWaiter](https://docs.rs/taskvisor/latest/taskvisor/core/struct.TaskWaiter.html) and [TaskOutcome](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html) | One direct final result, outside the event bus. | Application decisions.                  |
| [Subscribe](https://docs.rs/taskvisor/latest/taskvisor/subscribers/trait.Subscribe.html) and [Event](https://docs.rs/taskvisor/latest/taskvisor/events/struct.Event.html)     | Best-effort bounded delivery.                   | Logs, metrics, traces, and live status. |

A watched outcome is independent of event loss while the process and runtime remain alive.
It is not durable storage.
[TaskWaiter::wait](https://docs.rs/taskvisor/latest/taskvisor/core/struct.TaskWaiter.html#method.wait) can return [`OutcomeUnavailable`](https://docs.rs/taskvisor/latest/taskvisor/error/enum.RuntimeError.html#variant.OutcomeUnavailable) if its completion channel closes unexpectedly.
Dropping the waiter does not cancel the task.

```mermaid
flowchart TB
accTitle: Final outcomes and event delivery
accDescr: Direct final outcomes bypass the event bus and the bounded subscriber queues.
Runtime["Runtime and controller"]
Waiter["TaskWaiter"]
Bus["Event bus: drop oldest when full"]
Relay["Event relay"]
Queues["Subscriber queues: drop incoming when full"]
Callbacks["Subscriber callbacks"]
Runtime -->|"direct final outcome"| Waiter
Runtime -->|"lifecycle events"| Bus
Bus --> Relay
Relay --> Queues
Queues -->|"serial per subscriber"| Callbacks
```

The event path has two separate loss points. Neither controls the direct outcome channel.

## Separate API errors from task outcomes

An API error means the call did not return its documented success value.
It does not prove that no command or state change was committed. Check the contract of that method.
A [`TaskOutcome`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html) reports how watched work finally ended.

```mermaid
flowchart TB
accTitle: API errors and final task outcomes
accDescr: A watched add and its waiter return separate results at separate boundaries.
Add["add(spec).watch().execute().await"]
NoWaiter["No watched registration returned"]
Wait["waiter.wait().await"]
Unavailable["Result channel failed"]
Outcome["Work reached a final state"]
Add -->|"Err(RuntimeError)"| NoWaiter
Add -->|"Ok(waiter)"| Wait
Wait -->|"Err(OutcomeUnavailable)"| Unavailable
Wait -->|"Ok(TaskOutcome)"| Outcome
```

| Boundary                                                                                                                         | Type                                                                                               |
|----------------------------------------------------------------------------------------------------------------------------------|----------------------------------------------------------------------------------------------------|
| Checked configuration constructors and setters                                                                                   | [ConfigError](https://docs.rs/taskvisor/latest/taskvisor/core/enum.ConfigError.html)               |
| [`BackoffPolicy::new`](https://docs.rs/taskvisor/latest/taskvisor/policies/struct.BackoffPolicy.html#method.new)                 | [BackoffError](https://docs.rs/taskvisor/latest/taskvisor/policies/enum.BackoffError.html)         |
| [`SupervisorBuilder::try_build`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorBuilder.html#method.try_build) | [BuildError](https://docs.rs/taskvisor/latest/taskvisor/error/enum.BuildError.html)                |
| Runtime lifecycle, management, wait, and shutdown                                                                                | [RuntimeError](https://docs.rs/taskvisor/latest/taskvisor/error/enum.RuntimeError.html)            |
| Controller preparation and command intake                                                                                        | [ControllerError](https://docs.rs/taskvisor/latest/taskvisor/controller/enum.ControllerError.html) |
| One task attempt                                                                                                                 | [TaskError](https://docs.rs/taskvisor/latest/taskvisor/error/enum.TaskError.html)                  |
| Final result of watched work                                                                                                     | [`TaskOutcome`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html)             |
| Application code that combines runtime and controller calls                                                                      | [Error](https://docs.rs/taskvisor/latest/taskvisor/error/enum.Error.html)                          |

[`SupervisorBuilder::build`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorBuilder.html#method.build) and [`Supervisor::new`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.new) panic when checked construction would fail.
Use [try_build](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorBuilder.html#method.try_build) to handle typed construction errors.

[`Ok(TaskOutcome::Failed { .. })`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html#variant.Failed) means outcome delivery succeeded and the task ended in failure.
It is not an API error.
For controller work, `submit(request).watch().execute().await?` confirms command intake.
The waiter can later deliver [`TaskOutcome::Rejected`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html#variant.Rejected) if slot or registry admission fails.

## Handle final outcomes

Final outcomes distinguish:

| Outcome                                                                                                      | Meaning                                                                              |
|--------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------|
| [`Completed`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html#variant.Completed)       | The final attempt succeeded and the restart policy stopped the task.                 |
| [`Failed`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html#variant.Failed)             | Retryable failure stopped under policy or retry limit.                               |
| [`Fatal`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html#variant.Fatal)               | The task reported a permanent failure.                                               |
| [`Canceled`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html#variant.Canceled)         | Cancellation was requested or reported.                                              |
| [`ForceAborted`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html#variant.ForceAborted) | Taskvisor stopped waiting before the task stopped cooperatively.                     |
| [`Panicked`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html#variant.Panicked)         | The actor or protected attempt-owned cleanup panicked before final outcome delivery. |
| [`Rejected`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html#variant.Rejected)         | Admission rejected the work, or queued controller work was removed.                  |

Use [TaskOutcomeKind](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcomeKind.html) and [RejectionKind](https://docs.rs/taskvisor/latest/taskvisor/events/enum.RejectionKind.html) for branching, metrics, and alerts.
Treat reason strings as diagnostic text.
A panic while polling task code becomes a retryable task failure instead of [`Panicked`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html#variant.Panicked).
Removing watched controller work while it is still queued or waiting for registry-command capacity produces [`Rejected`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html#variant.Rejected) with [`RejectionKind::RemovedFromQueue`](https://docs.rs/taskvisor/latest/taskvisor/events/enum.RejectionKind.html#variant.RemovedFromQueue), not [`Canceled`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html#variant.Canceled).
Once work enters the registry, cancellation uses the normal task lifecycle even if the task is still waiting for a concurrency permit.

Taskvisor delivers the final outcome before deferred cleanup destroys the retained task object and actor result.
That later destruction can block or panic. It cannot change an outcome already delivered through [`TaskWaiter`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.TaskWaiter.html).
Failures on that later path are reported as best-effort runtime diagnostics, not a new [`TaskOutcome::Panicked`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html#variant.Panicked).

## Branch on a final outcome

Match typed variants for application decisions and use stable labels for telemetry.
Keep a fallback arm because [`TaskOutcome`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html) is non-exhaustive.

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

Use [TaskOutcome::kind](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html#method.kind) or typed variants for classification, not reason strings.

## Understand ForceAborted

[ForceAborted](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html#variant.ForceAborted) normally follows the configured grace period.
Last-owner fallback and signal-setup failure cleanup cannot wait for that period.
The physical attempt can remain active until synchronous task code returns control to Tokio.

## Treat events as observability

Choose the interface for the question:

| Need                                 | Interface                                                                                                                                                                         |
|--------------------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Application decision                 | [`TaskWaiter`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.TaskWaiter.html) and [`TaskOutcome`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html) |
| Readable demo or small-tool logs     | [LogWriter](https://docs.rs/taskvisor/latest/taskvisor/subscribers/struct.LogWriter.html) with `logging`.                                                                         |
| Structured service telemetry         | [TracingBridge](https://docs.rs/taskvisor/latest/taskvisor/subscribers/struct.TracingBridge.html) with `tracing`.                                                                 |
| Application-owned metrics            | A custom [`Subscribe`](https://docs.rs/taskvisor/latest/taskvisor/subscribers/trait.Subscribe.html) implementation                                                                |
| Registry membership                  | [list](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.list)                                                                                  |
| Physical attempt activity            | [alive_snapshot](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.alive_snapshot)                                                              |
| Retained values and cleanup pressure | [ownership_snapshot](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.ownership_snapshot)                                                      |
| Per-key admission state              | [controller_snapshot](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.controller_snapshot)                                                    |

The shared event bus and every subscriber queue are bounded.
Events can be lost at the shared bus or in an individual subscriber queue.
When the shared bus is full, it drops the oldest event and retains the newest one.
When one subscriber queue is full, Taskvisor drops the incoming event for that subscriber.
Callbacks run one at a time for each subscriber.

### Choose shared or dedicated callback execution

Subscriber callbacks are synchronous and run outside Tokio worker threads.
Choose the execution mode from the work done inside the callback:

- Use `Shared`, the default, for short, bounded, non-blocking work.
  Examples are an atomic counter as in [custom_subscriber.rs](../examples/custom_subscriber.rs), in-process metrics as in [metrics.rs](../examples/metrics.rs), or a non-blocking handoff.
- Use `SubscriberExecution::Dedicated` when a callback can block synchronously and must not delay other subscriber lanes.
  Override [`Subscribe::execution`](https://docs.rs/taskvisor/latest/taskvisor/subscribers/trait.Subscribe.html).
  Examples are standard-output writes in [LogWriter](https://docs.rs/taskvisor/latest/taskvisor/subscribers/struct.LogWriter.html) and synchronous dispatch in [TracingBridge](https://docs.rs/taskvisor/latest/taskvisor/subscribers/struct.TracingBridge.html).
- Do not perform async I/O, long blocking work, or substantial processing in either callback mode.
  Copy the required event fields and hand them to an application-owned bounded queue and worker.
- Do not use a subscriber for a result that application logic cannot lose.
  Use [`TaskWaiter`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.TaskWaiter.html) instead.

All `Shared` lanes use one fixed library-owned OS worker.
A blocked callback on that worker delays the other shared lanes.
Each `Dedicated` subscriber adds one native thread and can run concurrently with the shared worker and other dedicated subscribers.
Its own bounded queue can still overflow.

`Dedicated` is a blocking-isolation mode, not a faster callback mode.
In one Taskvisor 0.9.0 reference run on Linux/aarch64 with 14 logical CPUs, the matched short-callback fan-out benchmark measured this complete lifecycle-and-delivery boundary:

- current-thread: one `Dedicated` subscriber at 55.407 K tasks/s, eight `Dedicated` at 6.434 K tasks/s, and eight `Shared` at 48.200 K tasks/s;
- multi-thread with four workers: one `Dedicated` at 48.960 K tasks/s, eight `Dedicated` at 9.126 K tasks/s, and eight `Shared` at 53.909 K tasks/s.

The run establishes that eight `Dedicated` subscribers were slower for this measured workload.
It does not establish a portable ratio or that every dedicated callback workload is slower on every host.
The benchmark measures 256 watched task completions plus matching `TaskFinished` delivery to every short-callback subscriber; it is not a callback-only microbenchmark.
The [fan-out benchmark contract](../benches/README.md#subscriber-fan-out) defines the included and excluded work.

Taskvisor does not elastically add callback workers or retire them merely because they are idle.
A callback still running when the shared shutdown deadline expires may continue on its detached worker after shutdown returns.
Taskvisor does not wait for callback-worker thread-local destructors; they may also continue after shutdown returns.
Shutdown deadlines can also drop events.
[SubscriberOverflow](https://docs.rs/taskvisor/latest/taskvisor/events/enum.EventKind.html#variant.SubscriberOverflow) diagnostics report loss when possible.

## Correlate events

Match [`Event.kind`](https://docs.rs/taskvisor/latest/taskvisor/events/struct.Event.html#structfield.kind) before reading its optional fields.
The [Event API](https://docs.rs/taskvisor/latest/taskvisor/events/struct.Event.html) and [EventKind API](https://docs.rs/taskvisor/latest/taskvisor/events/enum.EventKind.html) list the fields for each event:

| Field                                                                                                                                                                                                                              | Meaning                                                                                                                                      |
|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------|
| [`id`](https://docs.rs/taskvisor/latest/taskvisor/events/struct.Event.html#structfield.id)                                                                                                                                         | The task or submission ID, when present. Use it to correlate one submission across controller and runtime events.                            |
| [`task`](https://docs.rs/taskvisor/latest/taskvisor/events/struct.Event.html#structfield.task)                                                                                                                                     | The event's subject: usually a task name, but a slot name for controller events. Diagnostics may use a subscriber or runtime component name. |
| [`outcome_kind`](https://docs.rs/taskvisor/latest/taskvisor/events/struct.Event.html#structfield.outcome_kind), [`rejection_kind`](https://docs.rs/taskvisor/latest/taskvisor/events/struct.Event.html#structfield.rejection_kind) | Typed final-outcome or rejection categories. Use their stable labels for telemetry.                                                          |
| [`seq`](https://docs.rs/taskvisor/latest/taskvisor/events/struct.Event.html#structfield.seq)                                                                                                                                       | Process-local construction order, not the order of concurrent effects. It does not survive a process restart.                                |

Do not treat [`task`](https://docs.rs/taskvisor/latest/taskvisor/events/struct.Event.html#structfield.task) as a task ID or parse [`reason`](https://docs.rs/taskvisor/latest/taskvisor/events/struct.Event.html#structfield.reason) as a stable category.
The [metrics example](../examples/metrics.rs) uses a `subject` label for [`task`](https://docs.rs/taskvisor/latest/taskvisor/events/struct.Event.html#structfield.task) and explains how to keep labels bounded.

See [outcomes.rs](../examples/outcomes.rs), [custom_subscriber.rs](../examples/custom_subscriber.rs), [logging.rs](../examples/logging.rs) (requires `logging`), [tracing.rs](../examples/tracing.rs) (requires `tracing`), and [metrics.rs](../examples/metrics.rs).

Source: [final outcomes](../src/core/outcome.rs), [event fields](../src/events/event.rs), [event bus](../src/events/bus.rs), [event relay](../src/core/runtime/event_relay.rs), and [subscriber delivery](../src/subscribers/subscriber_set.rs).
