---
title: Configure Taskvisor
description: Configure runtime limits, inherited task behavior, per-task overrides, subscriber queues, and keyed admission limits.
---

# Configure Taskvisor

## Configure each concern

Each configuration type has one role:

| Type                                                                                                                           | Controls                                 |
|--------------------------------------------------------------------------------------------------------------------------------|------------------------------------------|
| [SupervisorConfig](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorConfig.html)                               | Runtime limits and shutdown.             |
| [TaskDefaults](https://docs.rs/taskvisor/latest/taskvisor/core/struct.TaskDefaults.html)                                       | Task settings inherited at registration. |
| [TaskSpec](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskSpec.html)                                              | Settings for one task.                   |
| [ControllerConfig](https://docs.rs/taskvisor/latest/taskvisor/controller/struct.ControllerConfig.html)                         | Keyed-admission limits.                  |
| [Subscribe::queue_capacity](https://docs.rs/taskvisor/latest/taskvisor/subscribers/trait.Subscribe.html#method.queue_capacity) | One subscriber's event queue.            |

Runtime settings stay fixed after the supervisor is built.

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

## Inherit or override task settings

At registration, Taskvisor fills inherited settings from [`TaskDefaults`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.TaskDefaults.html).
The [`TaskSpec`](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskSpec.html) constructor decides which fields inherit:

| Constructor                                                                                                                                                                                                                                                                                                 | Restart policy             | Other settings                                                |
|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|----------------------------|---------------------------------------------------------------|
| [from_defaults](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskSpec.html#method.from_defaults)                                                                                                                                                                                                 | Inherited.                 | Backoff, timeout, and retry limit are inherited.              |
| [`once`](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskSpec.html#method.once), [`restartable`](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskSpec.html#method.restartable), [`periodic`](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskSpec.html#method.periodic) | Chosen by the constructor. | Backoff, timeout, and retry limit are inherited.              |
| [new](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskSpec.html#method.new)                                                                                                                                                                                                                     | Passed explicitly.         | Backoff and timeout are explicit; retries start as unlimited. |

A later `with_*` call overrides that field, including an inherited limit:

- [with_timeout(None)](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskSpec.html#method.with_timeout) disables the attempt timeout. [`Duration::ZERO`](https://doc.rust-lang.org/std/time/struct.Duration.html#associatedconstant.ZERO) has the same effect.
- [with_max_retries(None)](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskSpec.html#method.with_max_retries) allows unlimited retries. It does not disable retries.

To disable retries, use [`TaskSpec::once`](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskSpec.html#method.once) or [`RestartPolicy::Never`](https://docs.rs/taskvisor/latest/taskvisor/policies/enum.RestartPolicy.html#variant.Never).
See [Choose task behavior](lifecycle-policies.md) for the restart policies.

## Know the defaults

Main defaults:

| Setting                   | Default                                                                                                                                                      |
|---------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Graceful task shutdown    | 60 seconds.                                                                                                                                                  |
| Subscriber drain          | 5 seconds, shared by all subscriber queues.                                                                                                                  |
| Concurrent task attempts  | Unlimited.                                                                                                                                                   |
| Registered-task limit     | 1024.                                                                                                                                                        |
| Ownership capacity        | 1024 per supervisor across accepted tasks and subscribers.                                                                                                   |
| Event bus capacity        | 1024.                                                                                                                                                        |
| Subscriber queue capacity | 1024 per subscriber; override through [`queue_capacity`](https://docs.rs/taskvisor/latest/taskvisor/subscribers/trait.Subscribe.html#method.queue_capacity). |
| Registry command capacity | 1024.                                                                                                                                                        |
| Restart policy            | On retryable failure.                                                                                                                                        |
| Failure backoff           | 200 ms initial base, capped at 30 s, with equal jitter; the first delay is 100–200 ms.                                                                       |
| Attempt timeout           | None.                                                                                                                                                        |
| Failure retry limit       | Unlimited.                                                                                                                                                   |

## Bound different resources

Three limits answer different questions:

| Limit                                                                                                                              | What it bounds                                                                                        |
|------------------------------------------------------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------|
| [`max_concurrent`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorConfig.html#method.max_concurrent)             | Attempts physically running at the same time.                                                         |
| [`max_registered_tasks`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorConfig.html#method.max_registered_tasks) | Registered and removing tasks through terminal cleanup; force-aborted work can remain charged longer. |
| [`ownership_capacity`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorConfig.html#method.ownership_capacity)     | Accepted task and subscriber values still owned through physical cleanup.                             |

[`SupervisorConfig::with_ownership_capacity(None)`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorConfig.html#method.with_ownership_capacity) removes the ownership count bound.
Cleanup still uses a bounded worker set, but retained values and cleanup backlog can then grow without a count limit.
Use [`Supervisor::ownership_snapshot`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.ownership_snapshot) or [SupervisorHandle::ownership_snapshot](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.ownership_snapshot) to inspect configured and effective limits, available units, waiting requests, and deferred cleanup.

During cleanup handoff, one task can temporarily consume two [`max_registered_tasks`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorConfig.html#method.max_registered_tasks) units.

## Choose values for an application

Choose limits from the application's needs:

1. Decide which failures may repeat and whether retries need a finite limit.
2. Choose an attempt timeout only for operations that are safe to stop by dropping their future.
3. Bound concurrent attempts and registered tasks to fit the work and its dependencies.
4. Keep a finite ownership limit unless the application accepts an unbounded number of retained values and cleanup batches.
5. Choose task grace and subscriber drain deadlines that match the application's shutdown owner.
6. When using keyed admission, configure controller queues and tracked-slot limits separately.
7. Observe direct outcomes, best-effort events, and runtime snapshots at their documented boundaries.

## Observe ownership pressure

[OwnershipSnapshot](https://docs.rs/taskvisor/latest/taskvisor/core/struct.OwnershipSnapshot.html) separates admission pressure from deferred cleanup:

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

[`waiters`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.OwnershipSnapshot.html#structfield.waiters) counts ownership requests waiting for capacity.
[`cleanup_queued`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.OwnershipSnapshot.html#structfield.cleanup_queued) and [`cleanup_running`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.OwnershipSnapshot.html#structfield.cleanup_running) count batches waiting for or running on destructor workers.
[`retired()`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.OwnershipSnapshot.html#method.retired) reports permanent loss from a finite ownership capacity after destructor failure.
An [`available`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.OwnershipSnapshot.html#structfield.available) count of zero does not by itself mean the next waiting request will fail. Another owned value may release its unit.
Capacity fields are `None` when ownership admission is unlimited.
The snapshot combines separate reads and can become stale immediately.

Checked `try_with_*` methods accept raw integers and return [ConfigError](https://docs.rs/taskvisor/latest/taskvisor/core/enum.ConfigError.html) for invalid values.

Source: [runtime settings](../src/core/config.rs), [task defaults](../src/core/task_defaults.rs), [task-setting resolution](../src/tasks/spec.rs), and [ownership snapshot](../src/core/ownership.rs).
