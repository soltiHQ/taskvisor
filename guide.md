# Taskvisor user guide

This guide explains how to use Taskvisor in an application and how to choose between its public workflows. 
For exact method signatures, error variants, and edge-case contracts, use the [API documentation](https://docs.rs/taskvisor).

Taskvisor is an in-process runtime. Tasks, queued submissions, task IDs, events, and watched outcomes do not survive process exit. 
Use durable external storage when work must resume after a restart.

- New to Taskvisor? Run the [Quick start](README.md#quick-start).
- Looking for a complete program? Follow the [examples guide](examples/README.md).
- Changing Taskvisor itself? Start with the [contributor map](src/ARCHITECTURE.md).

## In this guide

- Start: [mental model](#mental-model), [installation](#install-taskvisor), [task definition](#define-a-task), and [task behavior](#choose-task-behavior).
- Run: [supervisor entry points](#choose-how-the-supervisor-runs), [runtime management](#manage-tasks-at-runtime), and [cancellation](#cancellation-and-shutdown).
- Extend: [outcomes and events](#final-outcomes-and-lifecycle-events), [per-key coordination](#coordinate-work-by-key), and [configuration](#configure-taskvisor).
- Deploy: [production boundaries](#production-boundaries) and [common mistakes](#common-mistakes).

## Mental model

A task defines executable work. A task specification gives that work a name and lifecycle policy.
The supervisor owns registration, attempts, cancellation, and cleanup.

```text
Task / TaskFn ──► TaskSpec
                      ├── add* ──► registry
                      └── ControllerSpec
                               └── submit* ──► controller
                                                    ├── admitted ──► registry
                                                    └── rejected ──► watched TaskWaiter

registry ──► supervised attempts
                 ├── watched ──► TaskWaiter
                 └── observed ─► Event subscribers
```

| Value              | Role                                                                |
|--------------------|---------------------------------------------------------------------|
| `Task` or `TaskFn` | Creates a fresh future for each attempt.                            |
| `TaskSpec`         | Gives work its registry name and execution policy.                  |
| `Supervisor`       | Owns one Taskvisor runtime.                                         |
| `SupervisorHandle` | Manages a running supervisor.                                       |
| `TaskId`           | Identifies one process-local registration or controller submission. |
| Task name          | Uniquely identifies registry membership inside one supervisor.      |
| Controller slot    | Coordinates submissions that must not own the same key together.    |
| `TaskWaiter`       | Delivers one direct in-process final outcome.                       |
| `Event`            | Describes lifecycle activity through best-effort delivery.          |

Direct `add*` methods send a `TaskSpec` to the runtime registry. 
Controller `submit*` methods first apply a per-slot admission policy, then hand admitted work to the same registry.

## Install Taskvisor

The default install includes the controller API:

```toml
taskvisor = "0.7"
```

The controller has no runtime effect until a supervisor is built with `with_controller`.

| Feature              | Default | Adds                                                   |
|----------------------|---------|--------------------------------------------------------|
| `controller`         | Yes.    | Slot-based admission control.                          |
| `tracing`            | No.     | `TracingBridge` for the `tracing` ecosystem.           |
| `logging`            | No.     | `LogWriter` for simple readable lifecycle output.      |
| `tokio-util-interop` | No.     | Access to the raw cancellation token in `TaskContext`. |
| `test-util`          | No.     | Constructors intended for external integration tests.  |

Enable an optional integration:

```toml
taskvisor = { version = "0.7", features = ["tracing"] }
```

Build without keyed admission:

```toml
taskvisor = { version = "0.7", default-features = false }
```

## Define a task

Use `TaskFn` for an async closure:

```rust
use taskvisor::{TaskFn, TaskRef};

let task: TaskRef = TaskFn::arc(|_ctx| async {
    println!("one attempt");
    Ok(())
});
```

Implement `Task` when a reusable type should hold state or dependencies across attempts. 
Each call to `Task::spawn` must return a fresh future. 
Keep synchronous work in `spawn` short; put the actual operation inside the returned future.

A shared `TaskRef` can back several registrations. Registrations that overlap in one supervisor need different names. 
A name can be reused after the earlier registration releases it. 
The registrations receive different task IDs, and their `spawn` calls may run concurrently when configured attempt capacity permits. 
Shared task state must support that use.

After a force-abort, Taskvisor may keep the name reserved until it observes that the task attempt has physically returned.

Keep blocking and CPU-heavy work away from Tokio worker threads. 
Use a suitable blocking executor, worker pool, or external runtime. 
Also keep the destructor of an attempt future short: Taskvisor drops that future synchronously when the attempt ends or is canceled.

Runnable examples:

- [basic.rs](examples/basic.rs) uses `TaskFn` for one static task;
- [task_type.rs](examples/task_type.rs) implements `Task` for reusable state;
- [queue_consumer.rs](examples/queue_consumer.rs) supervises a cancellation-aware receive loop;
- [cpu_job.rs](examples/cpu_job.rs) moves CPU work to Rayon and explains the cancellation limit.

## Choose task behavior

`TaskSpec` selects what follows success or a retryable failure:

| Constructor               | After success                       | After a retryable failure                                |
|---------------------------|-------------------------------------|----------------------------------------------------------|
| `TaskSpec::once`          | Stop.                               | Stop.                                                    |
| `TaskSpec::restartable`   | Stop.                               | Retry if the policy and retry limit allow.               |
| `TaskSpec::periodic`      | Wait after completion, then repeat. | Retry through failure backoff if the retry limit allows. |
| `TaskSpec::from_defaults` | Use `TaskDefaults`.                 | Use `TaskDefaults`.                                      |

One task ID runs attempts sequentially. Two attempts for that ID never overlap.

| Attempt result        | Meaning                                                              |
|-----------------------|----------------------------------------------------------------------|
| `Ok(())`              | Success. The restart policy decides whether another attempt follows. |
| `TaskError::Fail`     | Retryable failure.                                                   |
| `TaskError::Fatal`    | Permanent failure; stop without retry.                               |
| `TaskError::Canceled` | Cooperative cancellation; stop without retry.                        |
| Attempt timeout       | Retryable timeout failure.                                           |

With panic unwinding enabled, a panic while creating or polling the attempt future becomes a retryable failure. 
A panic during protected cleanup can instead produce a final `Panicked` outcome.
`panic = "abort"` cannot be caught.

A retry limit counts retries after the first failed attempt. 
A limit of three therefore allows at most four consecutive failed attempts. 
A successful attempt resets the failure streak.

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

A periodic interval starts after a successful attempt completes. 
It is fixed-delay scheduling, not a wall-clock or cron schedule.

See [periodic.rs](examples/periodic.rs), [restart_policies.rs](examples/restart_policies.rs), and [configuration.rs](examples/configuration.rs).

## Choose how the supervisor runs

Choose an entry point based on how tasks are supplied and who requests shutdown:

| Entry point                       | Use it when                                                 |
|-----------------------------------|-------------------------------------------------------------|
| `Supervisor::run`                 | The initial batch finishes naturally.                       |
| `Supervisor::run_until`           | The application owns the future that requests shutdown.     |
| `Supervisor::run_with_os_signals` | Taskvisor should install process signal handlers.           |
| `Supervisor::serve`               | Work is discovered or managed while the service is running. |

`run`, `run_until`, and `run_with_os_signals` submit one initial batch through all-or-nothing registry admission. 
Admission can reject the full batch; `run_until` can begin shutdown before the batch commits. 
Their return value describes the shared supervisor lifecycle and cleanup workflow; it does not mean every task succeeded. 
Use watched work when application logic needs each final result.

These three methods share one static lifecycle. 
After one commits, another static run on the same supervisor returns `RuntimeError::AlreadyRunning`.

`run` and `run_until` do not install operating-system signal handlers.
`run_with_os_signals` is the explicit process-wide opt-in. 
An embedded application that already owns signals should use `run_until` or request shutdown through a dynamic handle.

On Unix, dropping Taskvisor's signal listeners does not restore the default signal disposition.
The application remains responsible for signal handling after the method returns.

`serve` starts the same runtime without a static batch and returns a `SupervisorHandle`. 
It does not install signal handlers. 
Call `handle.shutdown().await` when the application wants the joined cleanup result.

Create a supervisor with `Supervisor::new` when runtime configuration and subscribers are enough.
Use `Supervisor::builder` when the application also needs task defaults, a controller, or typed construction errors through `try_build`.

Runnable entry-point examples:

- [basic.rs](examples/basic.rs) uses `run`;
- [application_shutdown.rs](examples/application_shutdown.rs) uses `run_until`;
- [graceful_worker.rs](examples/graceful_worker.rs) uses `run_with_os_signals`;
- [dynamic_tasks.rs](examples/dynamic_tasks.rs) uses `serve`.

## Manage tasks at runtime

A dynamic handle separates task registration from task completion.

| Operation          | What an `Ok` result means                                                                                                                            |
|--------------------|------------------------------------------------------------------------------------------------------------------------------------------------------|
| `add`              | The runtime registry accepted the task. The first attempt may not have started yet.                                                                  |
| `add_and_watch`    | Registration succeeded and the caller received a final-outcome waiter.                                                                               |
| `submit`           | The controller accepted the command. Slot admission happens later.                                                                                   |
| `submit_and_watch` | Command intake succeeded and the caller received a waiter for rejection or the admitted task's final outcome.                                        |
| `TaskWaiter::wait` | A direct final in-process outcome was delivered.                                                                                                     |
| `remove`           | The boolean says whether this call created the stop claim. Registered cleanup may continue; queued work is removed before return.                    |
| `cancel`           | The boolean says whether this call created the stop claim. For registered work, registry membership and the final outcome are settled before return. |

`false` can mean the work was unknown, already finished, or already claimed by another stop request. 
A `cancel` call that joins an existing removal waits for the same cleanup and also returns `false`.

Use `TaskId` for one exact registration or controller submission. 
Use `remove_by_name` and `cancel_by_name` for registered work addressed by task name. 
Controller work that is still queued does not own a registered task name; stop it with the task ID returned by `submit*`.

`list` returns registry membership. 
It includes tasks waiting for attempt capacity, in retry backoff, running, or completing cleanup. 
`alive_snapshot` and `is_alive` answer a different question: whether a physical attempt is still active. 
Both are point-in-time snapshots and may be stale as soon as concurrent work changes.

State-changing async methods wait for capacity at their command boundary. 
Their `try_*` variants fail immediately when the required capacity is unavailable. 
After command admission, they still wait for the normal decision. 
The exact boundary and error are documented on each method in the [API reference](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html).

See [dynamic_tasks.rs](examples/dynamic_tasks.rs) for one complete management flow.

## Cancellation and shutdown

Cancellation starts cooperatively. A resident task must observe `TaskContext`:

```rust
use taskvisor::{TaskContext, TaskError};

async fn do_work() -> Result<(), TaskError> {
    // Application work goes here.
    Ok(())
}

async fn run_one_operation(ctx: &TaskContext) -> Result<(), TaskError> {
    ctx.run_until_cancelled(do_work()).await?
}

async fn run_with_more_branches(ctx: &TaskContext) -> Result<(), TaskError> {
    tokio::select! {
        _ = ctx.cancelled() => Err(TaskError::Canceled),
        result = do_work() => result,
    }
}
```

`run_until_cancelled` drops the wrapped future when cancellation wins. 
Use it only when dropping that future is a safe way to cancel the exact operation. 
Check the operation's cancellation-safety contract; an external commit, acknowledgement, or partially consumed input may need an explicit protocol. 
The Tokio sleep in [graceful_worker.rs](examples/graceful_worker.rs) is a simple drop-safe example.

An attempt timeout also drops the attempt future. 
It does not undo side effects that already happened. 
A blocking future destructor can delay attempt release beyond the configured timeout.

The joined shutdown path:

1. Closes admission for new work.
2. Rejects pending controller work and requests cancellation for registered tasks.
3. Waits through the configured grace period.
4. Commits `ForceAborted` for tasks that did not stop in time.
5. Finishes runtime cleanup and drains subscriber queues up to their separate deadline.

Taskvisor cannot interrupt synchronous code in the middle of a poll. 
After the grace period, the final outcome may be `ForceAborted` while that synchronous code is still physically running. 
The supervisor keeps ownership until it returns.

`handle.shutdown().await` joins the shared shutdown workflow and returns its result. 
Dropping the final public owner can request cancellation, but a destructor cannot await cleanup or report its errors.

## Final outcomes and lifecycle events

Taskvisor has two result paths with different contracts:

| Path                           | Contract                                        | Use it for                              |
|--------------------------------|-------------------------------------------------|-----------------------------------------|
| `TaskWaiter` and `TaskOutcome` | One direct final result, outside the event bus. | Application decisions.                  |
| `Subscribe` and `Event`        | Best-effort bounded delivery.                   | Logs, metrics, traces, and live status. |

A watched outcome is independent of event loss while the process and runtime remain alive. 
It is not durable storage. 
`TaskWaiter::wait` can return `OutcomeUnavailable` if its completion channel closes unexpectedly.

Final outcomes distinguish:

| Outcome        | Meaning                                                                 |
|----------------|-------------------------------------------------------------------------|
| `Completed`    | The final attempt succeeded and the restart policy stopped the task.    |
| `Failed`       | Retryable failure stopped under policy or retry limit.                  |
| `Fatal`        | The task reported a permanent failure.                                  |
| `Canceled`     | Cancellation was requested or reported.                                 |
| `ForceAborted` | Cooperative stop did not finish within the allowed wait.                |
| `Panicked`     | The managed lifecycle or protected cleanup panicked.                    |
| `Rejected`     | Controller or registry admission rejected the work before its body ran. |

Use stable outcome and rejection kinds for branching, metrics, and alerts. 
Treat reason strings as diagnostic text.

The shared event bus and every subscriber queue are bounded. 
Events can be lost at the shared bus or in an individual subscriber queue. 
Each subscriber receives callbacks serially in its own order, but two different subscribers can run at the same time.

Subscriber callbacks are synchronous and run outside Tokio worker threads. Keep them short. 
Forward async or long blocking work to an application-owned queue. 
Overflow and shutdown deadlines can drop events; overflow diagnostics report loss when possible.

See [outcomes.rs](examples/outcomes.rs), [custom_subscriber.rs](examples/custom_subscriber.rs), [logging.rs](examples/logging.rs), [tracing.rs](examples/tracing.rs), and [metrics.rs](examples/metrics.rs).

## Coordinate work by key

This section requires the `controller` feature. 
It is enabled by default, but each supervisor must install a controller explicitly:

```rust
use taskvisor::{ControllerConfig, Supervisor, SupervisorConfig};

let _supervisor = Supervisor::builder(SupervisorConfig::default())
    .with_controller(ControllerConfig::default())
    .build();
```

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

| Policy          | Busy-slot behavior                                                                     |
|-----------------|----------------------------------------------------------------------------------------|
| `Queue`         | Append to the bounded FIFO queue. A later `Replace` can still displace the queue head. |
| `Replace`       | Request owner retirement and create or replace the queue head.                         |
| `DropIfRunning` | Reject the incoming submission without changing the owner or queue.                    |

A replacement is not guaranteed to become the next owner. 
A newer `Replace` can supersede it before admission, and later registry admission can still reject it.

```rust
use taskvisor::{ControllerSpec, TaskFn, TaskRef, TaskSpec};

let task: TaskRef = TaskFn::arc(|_ctx| async { Ok(()) });
let request = ControllerSpec::queue(TaskSpec::once("customer-42-job", task))
    .with_slot("customer-42");

assert_eq!(request.task_spec().name(), "customer-42-job");
assert_eq!(request.slot_name(), "customer-42");
```

`submit().await?` confirms command intake only. `submit_and_watch` returns a task ID and waiter. 
The waiter resolves to `Rejected` if admission fails or to the registered task's final outcome if admission succeeds.

`prepare_submission` allocates a task ID before intake. 
It does not reserve a name, slot, queue position, or runtime capacity.

`controller_snapshot` is a rolling diagnostic view. 
It reads slots independently and can already be stale when returned. 
Do not treat it as a transaction boundary.

Attempt timeout starts only after registry admission and after `Task::spawn` returns the attempt future. 
It does not limit time spent in a controller queue. Controller submission has no built-in end-to-end deadline.

Slots govern admission, not cancellation. 
There is no slot-wide cancel or remove operation. 
Stop queued work by task ID and registered work by task ID or task name.

`ControllerConfig` bounds command intake, per-slot queues, total pending work, tracked slots, registry-capacity waits, and concurrent identity operations. 
See its [API documentation](https://docs.rs/taskvisor/latest/taskvisor/controller/struct.ControllerConfig.html) for the exact defaults and rejection mapping.

Runnable controller examples:

- [controller_slots.rs](examples/controller_slots.rs) compares all three policies;
- [controller_admission.rs](examples/controller_admission.rs) watches admission and rejection;
- [tenant_sync.rs](examples/tenant_sync.rs) keeps the newest waiting revision per tenant.

## Configure Taskvisor

Configuration has four levels:

```text
SupervisorConfig ──► runtime-wide limits and shutdown
TaskDefaults ──────► inherited task behavior
TaskSpec ──────────► per-task overrides
ControllerConfig ──► keyed-admission limits
```

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

Main defaults:

| Setting                   | Default                                                    |
|---------------------------|------------------------------------------------------------|
| Graceful task shutdown    | 60 seconds.                                                |
| Subscriber drain          | 5 seconds, shared by all subscriber queues.                |
| Concurrent task attempts  | Unlimited.                                                 |
| Registered-task limit     | 1024.                                                      |
| Ownership capacity        | 1024 per supervisor across accepted tasks and subscribers. |
| Event bus capacity        | 1024.                                                      |
| Registry command capacity | 1024.                                                      |
| Restart policy            | On retryable failure.                                      |
| Failure backoff           | Exponential from 200 ms to 30 seconds with equal jitter.   |
| Attempt timeout           | None.                                                      |
| Failure retry limit       | Unlimited.                                                 |

Three limits answer different questions:

| Limit                  | What it bounds                                                                                        |
|------------------------|-------------------------------------------------------------------------------------------------------|
| `max_concurrent`       | Attempts physically running at the same time.                                                         |
| `max_registered_tasks` | Registered and removing tasks through terminal cleanup; force-aborted work can remain charged longer. |
| `ownership_capacity`   | Accepted task and subscriber values still owned through physical cleanup.                             |

`SupervisorConfig::with_ownership_capacity(None)` removes the ownership count bound. 
Cleanup still uses a bounded worker set, but retained values and cleanup backlog can then grow without a count limit.

Capacity values are non-zero where zero would make the runtime unusable. 
Checked `try_with_*`methods accept raw integers and return a configuration error for invalid values.

## Production boundaries

### In-process state

- Runtime state, task IDs, controller queues, watched outcomes, and events are not durable.
- Taskvisor does not recover work after process failure.
- A watched outcome belongs to the current caller and process.

### Cooperative cancellation

- Long-running tasks must observe `TaskContext`.
- Synchronous task code cannot be interrupted in the middle of a poll.
- `ForceAborted` can be delivered before the physical attempt returns.
- Attempt timeout drops the future but cannot undo external side effects.

### Best-effort observability

- The shared event bus and subscriber queues can drop events.
- Subscriber callbacks already running cannot be interrupted at the drain deadline.
- Use watched outcomes rather than events for application correctness.

### Scheduling and coordination scope

- Periodic work uses a delay after completion, not cron or missed-run recovery.
- Controller coordination is local to one supervisor.
- A controller slot is not a cancellation key.
- Supervisor-local budgets do not isolate operating-system CPU, memory, or thread limits.

### Owned user values

- Accepted tasks and configured subscribers consume ownership capacity through physical cleanup.
- Blocking user destructors occupy cleanup workers until they return.
- A panic in a user destructor permanently retires one unit from a finite ownership capacity.
- Removing the ownership limit allows retained user values and cleanup backlog to grow without a count bound.

The crate forbids unsafe Rust with `#![forbid(unsafe_code)]`.

## Common mistakes

- Treating `run().await == Ok(())` as proof that every task succeeded.
- Treating `submit().await?` as positive slot admission.
- Using best-effort events for application decisions.
- Forgetting to observe cancellation in a resident task.
- Treating a controller slot as a registered task name.
- Running blocking or CPU-heavy work on Tokio worker threads.
- Assuming a timeout or force-abort can undo external side effects.

## Continue learning

| Resource                                       | Next step                                          |
|------------------------------------------------|----------------------------------------------------|
| [Examples guide](examples/README.md)           | Choose a complete runnable scenario.               |
| [API documentation](https://docs.rs/taskvisor) | Read exact contracts for public types and methods. |
| [Benchmark guide](benches/README.md)           | Run and interpret the Criterion suites.            |
| [Contributor map](src/ARCHITECTURE.md)         | Follow runtime ownership and source boundaries.    |
