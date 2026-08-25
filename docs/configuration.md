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

The values in this example show where each setting belongs.
They are not capacity recommendations for every application.

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

## Choose values for an application

Work from the application contract instead of copying one set of numbers:

1. Decide which failures may repeat and whether retries need a finite limit.
2. Choose an attempt timeout only for operations that are safe to stop by dropping their future.
3. Bound concurrent attempts and registered tasks according to the work and dependencies they consume.
4. Keep finite ownership admission, or explicitly accept that retained user values and cleanup backlog can grow without a count bound.
5. Choose task grace and subscriber drain deadlines that match the application's shutdown owner.
6. When using keyed admission, configure controller queues and tracked-slot limits separately.
7. Observe direct outcomes, best-effort events, and runtime snapshots at their documented boundaries.

## Observe ownership pressure

The ownership snapshot distinguishes admission pressure from deferred cleanup:

```rust
use taskvisor::SupervisorHandle;

fn report_ownership(handle: &SupervisorHandle) {
    let state = handle.ownership_snapshot();

    println!(
        "in_use={:?} available={:?} waiters={} cleanup_queued={} \
         cleanup_running={} retired={:?} admission_open={}",
        state.in_use(),
        state.available,
        state.waiters,
        state.cleanup_queued,
        state.cleanup_running,
        state.retired(),
        state.admission_open,
    );
}
```

`waiters` counts ownership requests currently parked for capacity.
`cleanup_queued` and `cleanup_running` count deferred-cleanup batches on the isolated destructor path.
`retired()` reports permanent loss from a finite ownership capacity after destructor failure.
An `available` value of zero does not by itself prove that the next waiting request will fail; another lifetime may release its unit.
Capacity fields are `None` when ownership admission is unlimited.
The complete snapshot is rolling and can become stale immediately.

Capacity values are non-zero where zero would make the runtime unusable.
Checked `try_with_*` methods accept raw integers and return a configuration error for invalid values.
