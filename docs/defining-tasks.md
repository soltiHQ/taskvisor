---
title: Define a task
description: Define Taskvisor work with an async closure or a reusable task type.
---

# Define a task

## Use a closure for simple work

Use [`TaskFn::arc`](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskFn.html#method.arc) for an async closure:

```rust
use taskvisor::{TaskFn, TaskRef};

let task: TaskRef = TaskFn::arc(|_ctx| async {
    println!("one attempt");
    Ok(())
});
```

The [closure adapter](../src/tasks/impl/func.rs) calls the closure for each attempt.
The closure must create a fresh future each time.
Clone captured shared state into the returned future when needed.

## Implement Task for reusable work

Implement [`Task`](https://docs.rs/taskvisor/latest/taskvisor/tasks/trait.Task.html) when a reusable type should hold state or dependencies across attempts.
Each [`Task::spawn`](https://docs.rs/taskvisor/latest/taskvisor/tasks/trait.Task.html#tymethod.spawn) call must return a fresh future.
[`spawn`](https://docs.rs/taskvisor/latest/taskvisor/tasks/trait.Task.html#tymethod.spawn) runs synchronously, before the attempt timeout starts.
Keep it short. Put the actual operation inside the returned future.

```rust
use std::sync::Arc;
use std::time::Duration;
use taskvisor::{BoxTaskFuture, Task, TaskContext};

struct EndpointProbe {
    endpoint: Arc<str>,
}

impl Task for EndpointProbe {
    fn spawn(&self, ctx: TaskContext) -> BoxTaskFuture {
        let endpoint = Arc::clone(&self.endpoint);

        Box::pin(async move {
            ctx.run_until_cancelled(tokio::time::sleep(Duration::from_millis(100)))
                .await?;
            println!("checked {endpoint}");
            Ok(())
        })
    }
}
```

The task object keeps `endpoint` across attempts.
Each future owns a separate [`Arc`](https://doc.rust-lang.org/std/sync/struct.Arc.html) clone that points to the same endpoint string.
The [task contract](../src/tasks/task.rs) defines the boundary between the task object and its attempt futures.

## Reuse task values safely

A shared [`TaskRef`](https://docs.rs/taskvisor/latest/taskvisor/tasks/type.TaskRef.html) can back several registrations.
Registrations that overlap in one supervisor need different names.
A name can be reused after the earlier registration releases it.
The registrations receive different task IDs.
Their [`spawn`](https://docs.rs/taskvisor/latest/taskvisor/tasks/trait.Task.html#tymethod.spawn) calls may run concurrently when the configured attempt capacity allows it.
Shared task state must support that use.

After a force-abort, Taskvisor may keep the name reserved until it observes that the task attempt has physically returned.
See [logical completion and physical release](cancellation-and-shutdown.md#separate-logical-completion-from-physical-release).

## Keep blocking work off Tokio

Keep blocking and CPU-heavy work away from Tokio worker threads.
Use a suitable blocking executor, worker pool, or external runtime.
Keep the destructor of an attempt future short too.
The [attempt runner](../src/core/runner.rs) drops that future synchronously when it ends, times out, or is aborted.
A blocking destructor keeps the attempt active and holds any concurrency permit until it returns.
This is separate from [deferred cleanup of the retained task object](production-boundaries.md#owned-user-values).

## Run an example

Runnable examples:

- [basic.rs](../examples/basic.rs) uses [`TaskFn`](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskFn.html) for one static task;
- [task_type.rs](../examples/task_type.rs) implements [`Task`](https://docs.rs/taskvisor/latest/taskvisor/tasks/trait.Task.html) for reusable state;
- [queue_consumer.rs](../examples/queue_consumer.rs) supervises a cancellation-aware receive loop;
- [cpu_job.rs](../examples/cpu_job.rs) moves CPU work to Rayon and explains the cancellation limit.
