---
title: Define a task
description: Define Taskvisor work with an async closure or a reusable task type.
---

# Define a task

## Use a closure for simple work

Use `TaskFn` for an async closure:

```rust
use taskvisor::{TaskFn, TaskRef};

let task: TaskRef = TaskFn::arc(|_ctx| async {
    println!("one attempt");
    Ok(())
});
```

## Implement Task for reusable work

Implement `Task` when a reusable type should hold state or dependencies across attempts.
Each call to `Task::spawn` must return a fresh future.
Keep synchronous work in `spawn` short; put the actual operation inside the returned future.

## Reuse task values safely

A shared `TaskRef` can back several registrations. Registrations that overlap in one supervisor need different names.
A name can be reused after the earlier registration releases it.
The registrations receive different task IDs, and their `spawn` calls may run concurrently when configured attempt capacity permits.
Shared task state must support that use.

After a force-abort, Taskvisor may keep the name reserved until it observes that the task attempt has physically returned.

## Keep blocking work off Tokio

Keep blocking and CPU-heavy work away from Tokio worker threads.
Use a suitable blocking executor, worker pool, or external runtime.
Also keep the destructor of an attempt future short: Taskvisor drops that future synchronously when the attempt ends or is canceled.

## Run an example

Runnable examples:

- [basic.rs](../examples/basic.rs) uses `TaskFn` for one static task;
- [task_type.rs](../examples/task_type.rs) implements `Task` for reusable state;
- [queue_consumer.rs](../examples/queue_consumer.rs) supervises a cancellation-aware receive loop;
- [cpu_job.rs](../examples/cpu_job.rs) moves CPU work to Rayon and explains the cancellation limit.
