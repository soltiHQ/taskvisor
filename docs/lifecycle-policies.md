---
title: Choose task behavior
description: Configure success repetition, retryable failures, backoff, attempt timeouts, and retry limits.
---

# Choose task behavior

## Choose behavior after each attempt

[TaskSpec](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskSpec.html) selects what happens after success or a retryable failure:

| Constructor               | After success                         | After a retryable failure                                |
|---------------------------|---------------------------------------|----------------------------------------------------------|
| [once](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskSpec.html#method.once) | Stop. | Stop. |
| [restartable](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskSpec.html#method.restartable) | Stop. | Retry if the retry limit allows. |
| [periodic](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskSpec.html#method.periodic) | Repeat after the interval; see [periodic timing](#understand-periodic-timing) for zero. | Retry through failure backoff if the retry limit allows. |
| [from_defaults](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskSpec.html#method.from_defaults) | Use `TaskDefaults`. | Use `TaskDefaults`. |

One task ID runs attempts sequentially. Two attempts for that ID never overlap.
These constructors choose a [RestartPolicy](https://docs.rs/taskvisor/latest/taskvisor/policies/enum.RestartPolicy.html), or inherit it from `TaskDefaults`.
An explicit `with_restart` call can change that choice.

## Interpret attempt results

Each attempt returns `Ok(())` or a [TaskError](https://docs.rs/taskvisor/latest/taskvisor/error/enum.TaskError.html):

| Attempt result        | Meaning                                                                  |
|-----------------------|--------------------------------------------------------------------------|
| `Ok(())`              | Success. The restart policy decides whether another attempt follows.     |
| `TaskError::Fail`     | Retryable failure.                                                       |
| `TaskError::Timeout`  | Retryable timeout reported by task code.                                 |
| `TaskError::Fatal`    | Permanent failure; stop without retry.                                   |
| `TaskError::Canceled` | Cooperative cancellation; stop without retry.                            |
| Configured timeout    | Drop the attempt future; report a retryable timeout if cleanup succeeds. |

A returned `TaskError::Timeout` produces the ordinary `AttemptFailed` event.
A configured attempt deadline drops the attempt future.
If cleanup succeeds, it produces [AttemptTimedOut](https://docs.rs/taskvisor/latest/taskvisor/events/enum.EventKind.html#variant.AttemptTimedOut) and a retryable timeout.
Both timeout failures follow the restart policy and retry limit.
If dropping the attempt future panics, Taskvisor instead produces `AttemptFailed` and ends with a final `Panicked` outcome without retrying.

With panic unwinding enabled, a panic while creating or polling the attempt future becomes a retryable failure.
A panic while destroying attempt-owned data inside the actor can instead produce a final `Panicked` outcome.
Later deferred destruction of the retained task object cannot change an outcome already delivered.
`panic = "abort"` cannot be caught.

See [Final outcomes and lifecycle events](outcomes-and-events.md) for final results, and [Cancellation and shutdown](cancellation-and-shutdown.md) for timeout limits.

## Make repeated attempts safe

A restartable task can run again after a retryable failure, timeout, or caught task panic.
Taskvisor cannot know whether an external system committed a side effect before the failure.
Return a retryable result only when repeating the attempt is acceptable.

| Application decision                                       | Attempt result        |
|------------------------------------------------------------|-----------------------|
| Temporary failure and repeating the operation is safe      | `TaskError::Fail`     |
| Task-reported deadline and repeating the operation is safe | `TaskError::Timeout`  |
| The application classifies the failure as permanent        | `TaskError::Fatal`    |
| Cancellation was observed and cooperative cleanup finished | `TaskError::Canceled` |
| The attempt completed its required work                    | `Ok(())`              |

For external side effects, use an application-owned idempotency key, transaction, reconciliation read, deduplication record, or acknowledgement protocol as appropriate.
Taskvisor manages retries. It does not provide rollback, durable execution, or exactly-once execution.

## Bound failure retries

A retry limit set with [with_max_retries](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskSpec.html#method.with_max_retries) counts retries after the first failed attempt.
A limit of three therefore allows at most four consecutive failed attempts.
A successful attempt resets the failure streak.

## Configure retry timing

[BackoffPolicy](https://docs.rs/taskvisor/latest/taskvisor/policies/struct.BackoffPolicy.html) sets the delay after retryable failures.
It does not set the delay after success.

```rust
use std::num::NonZeroU32;
use std::time::Duration;
use taskvisor::{BackoffPolicy, JitterPolicy, TaskRef, TaskSpec};

fn supervised(name: &str, task: TaskRef) -> TaskSpec {
    TaskSpec::restartable(name, task)
        .with_backoff(
            BackoffPolicy::exponential(Duration::from_millis(200))
                .with_max(Duration::from_secs(30))
                .with_jitter(JitterPolicy::Equal),
        )
        .with_timeout(Duration::from_secs(10))
        .with_max_retries(NonZeroU32::new(3).unwrap())
}
```

[Equal jitter](https://docs.rs/taskvisor/latest/taskvisor/policies/enum.JitterPolicy.html#variant.Equal) chooses a delay between half of the current base delay and the full base delay.
This spreads retries that would otherwise happen together.
Per-task settings override values inherited from `TaskDefaults`; see [configuration inheritance](configuration.md#inherit-or-override-task-settings).

## Understand periodic timing

A non-zero periodic interval starts after a successful attempt completes.
It is fixed-delay scheduling, not a wall-clock or cron schedule.
Passing `Duration::ZERO` removes the configured interval; Taskvisor still applies its internal fast-loop guard.

See [periodic.rs](../examples/periodic.rs), [restart_policies.rs](../examples/restart_policies.rs), and [configuration.rs](../examples/configuration.rs).

Source: [attempt loop](../src/core/actor.rs), [one-attempt execution](../src/core/runner.rs), and [backoff calculation](../src/policies/backoff.rs).
