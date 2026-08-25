---
title: Coordinate work by key
description: Queue, replace, or reject competing work through Taskvisor controller slots.
---

# Coordinate work by key

## Enable the controller

This section requires the `controller` feature.
It is enabled by default, but each supervisor must install a controller explicitly:

```rust
use taskvisor::{ControllerConfig, Supervisor, SupervisorConfig};

let _supervisor = Supervisor::builder(SupervisorConfig::default())
    .with_controller(ControllerConfig::default())
    .build();
```

## Separate IDs, names, and slots

Direct `add*` methods bypass controller admission.
`submit*` methods accept a `ControllerSpec`, apply its slot policy, and hand admitted work to the runtime registry.

| Identity        | Scope                                                    |
|-----------------|----------------------------------------------------------|
| `TaskId`        | One process-local registration or controller submission. |
| Task name       | Registry key inside one supervisor.                      |
| Controller slot | Admission key inside one supervisor controller.          |

Different task names can share a slot.
Without an explicit `with_slot`, the task name is also the slot.
A queued submission owns its task ID but does not own a registered task name yet.

A controller slot can remain occupied while admission, task execution, or physical release is pending.
An occupied slot does not always mean that a task body is currently polling.

## Choose a busy-slot policy

| Policy          | Busy-slot behavior                                                                     |
|-----------------|----------------------------------------------------------------------------------------|
| `Queue`         | Append to the bounded FIFO queue. A later `Replace` can still displace the queue head. |
| `Replace`       | Request owner retirement and create or replace the queue head.                         |
| `DropIfRunning` | Reject the incoming submission without changing the owner or queue.                    |

A replacement is not guaranteed to become the next owner.
A newer `Replace` can supersede it before admission, and later registry admission can still reject it.
`Replace` changes only the queue head and preserves the FIFO tail.
It does not use the per-slot `max_slot_queue` limit, but creating a new head can still reach `max_total_pending`.

```rust
use taskvisor::{ControllerSpec, TaskFn, TaskRef, TaskSpec};

let task: TaskRef = TaskFn::arc(|_ctx| async { Ok(()) });
let request = ControllerSpec::queue(TaskSpec::once("customer-42-job", task))
    .with_slot("customer-42");

assert_eq!(request.task_spec().name(), "customer-42-job");
assert_eq!(request.slot_name(), "customer-42");
```

## Watch admission and completion

`submit().await?` confirms command intake only. `submit_and_watch` returns a task ID and waiter.
The waiter resolves to `Rejected` if admission fails or to the registered task's final outcome if admission succeeds.

`prepare_submission` allocates a task ID before intake.
It does not reserve a name, slot, queue position, or runtime capacity.

## Read diagnostic state

`controller_snapshot` is a rolling diagnostic view.
It reads slots independently and can already be stale when returned.
Do not treat it as a transaction boundary.

## Know timeout and cancellation scope

Attempt timeout starts only after registry admission and after `Task::spawn` returns the attempt future.
It does not limit time spent in a controller queue. Controller submission has no built-in end-to-end deadline.

`submit_with_ownership_timeout` and `submit_and_watch_with_ownership_timeout` bound only the wait for
cleanup ownership before controller command intake. The deadline stops after Taskvisor acquires the ownership
permit. It does not cover controller-command capacity, a busy-slot queue, slot admission, registry-command
capacity, task execution, or final outcome delivery. The same boundary applies to the prepared submission methods.
A prepared value is consumed by a timeout, but its reserved ID remains silent because no command or lifecycle
event is produced.

Slots govern admission, not cancellation.
There is no slot-wide cancel or remove operation.
Stop queued work by task ID and registered work by task ID or task name.
Removing or canceling controller work that is still queued or waiting for registry-command capacity removes it directly before it runs.
Its watcher resolves to `Rejected` with `RejectionKind::RemovedFromQueue`, not to `Canceled`.

## Bound controller resources

`ControllerConfig` bounds command intake, per-slot queues, total pending work, tracked slots, registry-capacity waits, and concurrent identity operations.
See its [API documentation](https://docs.rs/taskvisor/latest/taskvisor/controller/struct.ControllerConfig.html) for the exact defaults and rejection mapping.

Runnable controller examples:

- [controller_slots.rs](../examples/controller_slots.rs) compares all three policies;
- [controller_admission.rs](../examples/controller_admission.rs) watches admission and rejection;
- [tenant_sync.rs](../examples/tenant_sync.rs) keeps the newest waiting revision per tenant.
