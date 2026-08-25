---
title: Configure Taskvisor
description: Configure runtime limits, inherited task behavior, per-task overrides, subscriber queues, and keyed admission limits.
---

# Configure Taskvisor

## Configure each concern

Configuration is split by concern:

```text
SupervisorConfig ──► runtime-wide limits and shutdown
TaskDefaults ──────► inherited task behavior
TaskSpec ──────────► per-task overrides
ControllerConfig ──► keyed-admission limits
Subscribe ─────────► per-subscriber event queue capacity
```

## Set runtime and task defaults

```rust
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::Arc;
use std::time::Duration;
use taskvisor::{Supervisor, SupervisorConfig, TaskDefaults};

fn configured_supervisor() -> Arc<Supervisor> {
    let runtime = SupervisorConfig::default()
        .with_grace(Duration::from_secs(30))
        .with_subscriber_shutdown_timeout(Duration::from_secs(5))
        .with_max_concurrent(NonZeroUsize::new(16))
        .with_ownership_capacity(NonZeroUsize::new(4096));

    let tasks = TaskDefaults::default()
        .with_timeout(Duration::from_secs(20))
        .with_max_retries(NonZeroU32::new(5).unwrap());

    Supervisor::builder(runtime)
        .with_task_defaults(tasks)
        .build()
}
```

## Know the defaults

Main defaults:

| Setting                   | Default                                                                                |
|---------------------------|----------------------------------------------------------------------------------------|
| Graceful task shutdown    | 60 seconds.                                                                            |
| Subscriber drain          | 5 seconds, shared by all subscriber queues.                                            |
| Concurrent task attempts  | Unlimited.                                                                             |
| Registered-task limit     | 1024.                                                                                  |
| Ownership capacity        | 1024 per supervisor across accepted tasks and subscribers.                             |
| Event bus capacity        | 1024.                                                                                  |
| Subscriber queue capacity | 1024 per subscriber; override through `queue_capacity`.                                |
| Registry command capacity | 1024.                                                                                  |
| Restart policy            | On retryable failure.                                                                  |
| Failure backoff           | 200 ms initial base, capped at 30 s, with equal jitter; the first delay is 100–200 ms. |
| Attempt timeout           | None.                                                                                  |
| Failure retry limit       | Unlimited.                                                                             |

## Bound different resources

Three limits answer different questions:

| Limit                  | What it bounds                                                                                        |
|------------------------|-------------------------------------------------------------------------------------------------------|
| `max_concurrent`       | Attempts physically running at the same time.                                                         |
| `max_registered_tasks` | Registered and removing tasks through terminal cleanup; force-aborted work can remain charged longer. |
| `ownership_capacity`   | Accepted task and subscriber values still owned through physical cleanup.                             |

`SupervisorConfig::with_ownership_capacity(None)` removes the ownership count bound.
Cleanup still uses a bounded worker set, but retained values and cleanup backlog can then grow without a count limit.
Use `Supervisor::ownership_snapshot` or `SupervisorHandle::ownership_snapshot` to inspect the
configured and effective limits, available units, parked requests, and deferred-cleanup activity.

During cleanup handoff, one task can temporarily consume two `max_registered_tasks` units.

Capacity values are non-zero where zero would make the runtime unusable.
Checked `try_with_*` methods accept raw integers and return a configuration error for invalid values.
