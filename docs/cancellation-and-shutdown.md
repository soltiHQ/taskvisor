---
title: Cancellation and shutdown
description: Make task operations cancellation-aware and join Taskvisor's bounded shutdown workflow.
---

# Cancellation and shutdown

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

An attempt timeout also drops the attempt future.
It does not undo side effects that already happened.
A blocking future destructor can delay attempt release beyond the configured timeout.

`cancel_with_timeout` and `cancel_by_name_with_timeout` limit how long the caller waits for registered task cleanup.
Controller ordering, command-queue admission, and the registry claim happen outside that timer.
A timeout stops this caller's wait; it does not undo cancellation or change the supervisor grace period.
If task completion is observed at the timeout boundary, completion wins.
Queued controller work is removed directly, and `cancel_with_timeout` does not apply its wait timer to that path.
A watched queued submission then resolves to `Rejected` with `RejectionKind::RemovedFromQueue`, not to `Canceled`.
The matching `try_*` methods make command-queue admission fail fast; their remaining behavior is unchanged.

The joined shutdown workflow has concurrent parts:

- It closes admission and signals runtime and controller shutdown.
- The registry requests cancellation for registered tasks, waits through the configured grace period, and commits `ForceAborted` for tasks that did not stop in time.
- The controller rejects pending submissions as its loop exits; this can overlap the registry grace period.
- Taskvisor joins the remaining runtime and controller cleanup, then drains subscriber queues up to their separate deadline.

Taskvisor cannot interrupt synchronous code in the middle of a poll.
After the grace period, the final outcome may be `ForceAborted` while that synchronous code is still physically running.
The supervisor keeps ownership until it returns.

`handle.shutdown().await` joins the shared shutdown workflow and returns its result.
Dropping the final public owner can request cancellation, but a destructor cannot await cleanup or report its errors.
