---
title: Install Taskvisor
description: Install Taskvisor and select the controller, observability, interop, or test features needed by an application.
---

# Install Taskvisor

Taskvisor requires Rust 1.90 or newer.
Add the [taskvisor crate](https://crates.io/crates/taskvisor) to your dependencies:

```toml
taskvisor = "0.8"
```

The default install includes the controller API.
To use it, build the supervisor with [with_controller](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorBuilder.html#method.with_controller).

| Feature              | Default | Adds                                                                                                                             |
|----------------------|---------|----------------------------------------------------------------------------------------------------------------------------------|
| `controller`         | Yes     | Slot-based admission control.                                                                                                    |
| `tracing`            | No      | [TracingBridge](https://docs.rs/taskvisor/latest/taskvisor/subscribers/struct.TracingBridge.html) for structured events.         |
| `logging`            | No      | [LogWriter](https://docs.rs/taskvisor/latest/taskvisor/subscribers/struct.LogWriter.html) for readable lifecycle output.         |
| `tokio-util-interop` | No      | Access to the raw cancellation token in [TaskContext](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskContext.html). |
| `test-util`          | No      | Constructors intended for external integration tests.                                                                            |

Enable an optional integration:

```toml
taskvisor = { version = "0.8", features = ["tracing"] }
```

Build without keyed admission:

```toml
taskvisor = { version = "0.8", default-features = false }
```

## Test helpers

With `test-util` enabled, [`TaskContext::detached`](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskContext.html#method.detached) and [`TaskContext::detached_cancelled`](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskContext.html#method.detached_cancelled) create contexts for direct task-code tests.
[`TaskId::for_tests`](https://docs.rs/taskvisor/latest/taskvisor/identity/struct.TaskId.html#method.for_tests) creates a fresh process-local ID.
[`TaskOutcome::failed_for_tests`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html#method.failed_for_tests), [`TaskOutcome::fatal_for_tests`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html#method.fatal_for_tests), and [`TaskOutcome::rejected_for_tests`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html#method.rejected_for_tests) create failure and rejection values for assertions.

Add the test-only feature and the Tokio test runtime:

```toml
[dev-dependencies]
taskvisor = { version = "0.8", features = ["test-util"] }
tokio = { version = "1", features = ["macros", "rt"] }
```

Test cancellation-aware application code without starting a supervisor:

```rust
use taskvisor::{TaskContext, TaskError};

async fn waits_for_work(ctx: &TaskContext) -> Result<(), TaskError> {
    ctx.run_until_cancelled(std::future::pending::<()>()).await?;
    Ok(())
}

#[tokio::test]
async fn observes_cancellation() {
    let ctx = TaskContext::detached_cancelled();

    assert!(matches!(
        waits_for_work(&ctx).await,
        Err(TaskError::Canceled)
    ));
}
```

For integration tests, start a supervisor with [`serve`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.serve), add watched work, assert its [`TaskOutcome`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html), then join [`shutdown`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html#method.shutdown).
See [TaskContext](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskContext.html) and [TaskOutcome](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html) for the test helpers.

Source: [Cargo features](../Cargo.toml) and [test contexts](../src/tasks/context.rs).
