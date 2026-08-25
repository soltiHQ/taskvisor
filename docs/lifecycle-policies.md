---
title: Choose task behavior
description: Configure success repetition, retryable failures, backoff, attempt timeouts, and retry limits.
---

# Choose task behavior

## Choose behavior after each attempt

`TaskSpec` selects what follows success or a retryable failure:

| Constructor               | After success                         | After a retryable failure                                |
|---------------------------|---------------------------------------|----------------------------------------------------------|
| `TaskSpec::once`          | Stop.                                 | Stop.                                                    |
| `TaskSpec::restartable`   | Stop.                                 | Retry if the policy and retry limit allow.               |
| `TaskSpec::periodic`      | Repeat; wait for a non-zero interval. | Retry through failure backoff if the retry limit allows. |
| `TaskSpec::from_defaults` | Use `TaskDefaults`.                   | Use `TaskDefaults`.                                      |

One task ID runs attempts sequentially. Two attempts for that ID never overlap.

## Interpret attempt results

| Attempt result        | Meaning                                                                  |
|-----------------------|--------------------------------------------------------------------------|
| `Ok(())`              | Success. The restart policy decides whether another attempt follows.     |
| `TaskError::Fail`     | Retryable failure.                                                       |
| `TaskError::Timeout`  | Retryable timeout reported by task code.                                 |
| `TaskError::Fatal`    | Permanent failure; stop without retry.                                   |
| `TaskError::Canceled` | Cooperative cancellation; stop without retry.                            |
| Configured timeout    | Drop the attempt future; report a retryable timeout if cleanup succeeds. |

A returned `TaskError::Timeout` follows the ordinary attempt-failure event path.
A configured attempt deadline drops the attempt future.
If cleanup succeeds, it produces the distinct `AttemptTimedOut` lifecycle event and a retryable timeout.
These two timeout failures remain subject to the restart policy and retry limit.
If dropping the attempt future panics, Taskvisor instead produces `AttemptFailed` and ends with a final `Panicked` outcome without retrying.

With panic unwinding enabled, a panic while creating or polling the attempt future becomes a retryable failure.
A panic while Taskvisor destroys attempt-owned data inside the physical actor boundary can instead produce a final `Panicked` outcome.
This does not include later deferred destruction of the retained task object after terminal outcome delivery.
`panic = "abort"` cannot be caught.

## Make repeated attempts safe

A restartable task can execute its attempt body again after `TaskError::Fail`, `TaskError::Timeout`, a configured attempt timeout, or a caught panic while creating or polling task code.
Taskvisor cannot determine whether an external system committed a side effect before that failure.
Return a retryable result only when running the attempt again is acceptable.

| Application decision                                      | Attempt result             |
|-----------------------------------------------------------|----------------------------|
| Temporary failure and repeating the operation is safe     | `TaskError::Fail`          |
| Task-reported deadline and repeating the operation is safe | `TaskError::Timeout`       |
| The application classifies the failure as permanent       | `TaskError::Fatal`         |
| Cancellation was observed and cooperative cleanup finished | `TaskError::Canceled`      |
| The attempt completed its required work                    | `Ok(())`                   |

For external side effects, use an application-owned idempotency key, transaction, reconciliation read, deduplication record, or acknowledgement protocol as appropriate.
Taskvisor provides retry lifecycle. It does not provide rollback, durable execution, or exactly-once execution.

## Bound failure retries

A retry limit counts retries after the first failed attempt.
A limit of three therefore allows at most four consecutive failed attempts.
A successful attempt resets the failure streak.

## Configure retry timing

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

Equal jitter chooses a delay between half of the current base delay and the full base delay.
This spreads retries that would otherwise happen together.
Per-task settings override values inherited from `TaskDefaults`.

## Understand periodic timing

A non-zero periodic interval starts after a successful attempt completes.
It is fixed-delay scheduling, not a wall-clock or cron schedule.
Passing `Duration::ZERO` removes the configured interval; Taskvisor still applies its internal fast-loop guard.

See [periodic.rs](../examples/periodic.rs), [restart_policies.rs](../examples/restart_policies.rs), and [configuration.rs](../examples/configuration.rs).
