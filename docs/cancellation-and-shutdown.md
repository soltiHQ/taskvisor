---
title: Cancellation and shutdown
description: Make task operations cancellation-aware and join Taskvisor's bounded shutdown workflow.
---

# Cancellation and shutdown

## Make operations cancellation-aware

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
Cancellation wins a tie, and an already-cancelled context does not poll the wrapped future.
Use it only when dropping that future is a safe way to cancel the exact operation.
Check the operation's cancellation-safety contract; an external commit, acknowledgement, or partially consumed input may need an explicit protocol.
The Tokio sleep in [graceful_worker.rs](../examples/graceful_worker.rs) is a simple drop-safe example.

## Know which deadline you are setting

Taskvisor deadlines cover different waits:

| API or setting                                      | What it bounds                                      | What expiry means                                                                                           |
|-----------------------------------------------------|-----------------------------------------------------|--------------------------------------------------------------------------------------------------------------|
| `TaskSpec::with_timeout` or `TaskDefaults::with_timeout` | One attempt after `Task::spawn` returns its future. | Cancel the attempt context and destroy the future; the timeout remains retryable under policy if cleanup succeeds. |
| `add*_with_ownership_timeout`                       | Ownership admission before registry command commit. | Return `RuntimeError::OwnershipAdmissionTimeout`; start no task and publish no lifecycle event for the request. |
| `submit*_with_ownership_timeout`                    | Ownership admission before controller command intake. | Return `ControllerError::OwnershipAdmissionTimeout`; later queues, admission, execution, and outcome remain outside the deadline. |
| `cancel*_with_timeout`                              | This caller's terminal wait after the registry claim. | Return `RuntimeError::TaskTerminationTimeout`; the cancellation request remains active.                     |
| `SupervisorConfig::with_grace`                      | Shared task cleanup during shutdown.                | Commit logical force-abort where needed and return `RuntimeError::GraceExceeded`; physical code may remain active. |
| `SupervisorConfig::with_subscriber_shutdown_timeout` | The separate shared drain of subscriber queues.    | Drop remaining queued events after the deadline; a callback already running cannot be interrupted.          |

Both ownership-admission timeouts happen before command intake.
Neither starts work or publishes a lifecycle event for the request.
These deadlines are not interchangeable.
None rolls back external side effects or interrupts synchronous Rust code in the middle of a poll.

## Understand attempt timeouts

An attempt timeout also drops the attempt future.
It does not undo side effects that already happened.
A blocking future destructor can delay attempt release beyond the configured timeout.

## Cancel work with a caller deadline

`cancel_with_timeout` and `cancel_by_name_with_timeout` limit how long the caller waits for registered task cleanup.
Controller ordering, command-queue admission, and the registry claim happen outside that timer.
A timeout stops this caller's wait; it does not undo cancellation or change the supervisor grace period.
If task completion is observed at the timeout boundary, completion wins.
Queued controller work is removed directly, and `cancel_with_timeout` does not apply its wait timer to that path.
A watched queued submission then resolves to `Rejected` with `RejectionKind::RemovedFromQueue`, not to `Canceled`.
The matching `try_*` methods make command-queue admission fail fast; their remaining behavior is unchanged.
If the caller already holds a `TaskWaiter`, a `TaskTerminationTimeout` neither consumes nor cancels it.
The waiter can still deliver the eventual terminal outcome.

## Join shutdown

The joined shutdown workflow has concurrent parts:

- It closes admission and signals runtime and controller shutdown.
- The registry requests cancellation for registered tasks, waits through the configured grace period, and commits `ForceAborted` for tasks that did not stop in time.
- The controller rejects pending submissions as its loop exits; this can overlap the registry grace period.
- Taskvisor joins the remaining runtime and controller cleanup, then drains subscriber queues up to their separate deadline.

Taskvisor cannot interrupt synchronous code in the middle of a poll.
After the grace period, the final outcome may be `ForceAborted` while that synchronous code is still physically running.
Force-aborted work remains visible through `alive_snapshot` until the physical attempt exits.
Its ownership unit remains charged through final isolated destruction.
A destructor panic permanently retires that unit from finite capacity instead of releasing it.

## Separate logical completion from physical release

Taskvisor exposes different views for different lifecycle boundaries:

| Question                                           | Interface                            |
|----------------------------------------------------|--------------------------------------|
| What final logical result did watched work reach?  | `TaskWaiter` and `TaskOutcome`       |
| Is the task still registered?                      | `list`                               |
| Is a physical attempt still active?                | `alive_snapshot` and `is_alive`      |
| Are user values retained or awaiting destruction?  | `ownership_snapshot`                 |

After `ForceAborted`, a task can be absent from `list` while its name remains in `alive_snapshot`.
After that attempt exits, the name can disappear from `alive_snapshot` while `ownership_snapshot` still reports in-use ownership, deferred-cleanup batches, or retired capacity.
These views are point-in-time diagnostics, not one atomic per-task record.
See [Configure Taskvisor](configuration.md#observe-ownership-pressure) for the ownership fields and [Final outcomes and lifecycle events](outcomes-and-events.md) for outcome semantics.

`handle.shutdown().await` joins the shared shutdown workflow and returns its result.
It affects the shared runtime and every handle clone; concurrent and later shutdown callers receive the cached shared result.
After the shutdown future is first polled, dropping that caller's future does not stop the detached cleanup operation.
Return does not prove that force-aborted synchronous code, a subscriber callback already running, or an isolated user destructor has finished.
Dropping the final public owner can request cancellation, but a destructor cannot await cleanup or report its errors.
