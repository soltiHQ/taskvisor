---
title: Install Taskvisor
description: Install Taskvisor and select the controller, observability, interop, or test features needed by an application.
---

# Install Taskvisor

The default install includes the controller API:

```toml
taskvisor = "0.8"
```

The controller has no runtime effect until a supervisor is built with `with_controller`.

| Feature              | Default | Adds                                                   |
|----------------------|---------|--------------------------------------------------------|
| `controller`         | Yes     | Slot-based admission control.                          |
| `tracing`            | No      | `TracingBridge` for the `tracing` ecosystem.           |
| `logging`            | No      | `LogWriter` for simple readable lifecycle output.      |
| `tokio-util-interop` | No      | Access to the raw cancellation token in `TaskContext`. |
| `test-util`          | No      | Constructors intended for external integration tests.  |

Enable an optional integration:

```toml
taskvisor = { version = "0.8", features = ["tracing"] }
```

Build without keyed admission:

```toml
taskvisor = { version = "0.8", default-features = false }
```

## Test helpers

With `test-util` enabled, `TaskContext::detached` and `TaskContext::detached_cancelled` create contexts for direct task-code tests.
`TaskId::for_tests` creates a fresh process-local ID.
`TaskOutcome::failed_for_tests`, `TaskOutcome::fatal_for_tests`, and `TaskOutcome::rejected_for_tests` construct non-exhaustive outcome variants for assertions.

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

For integration tests, start a supervisor with `serve`, add watched work, assert its `TaskOutcome`, then join `shutdown`.
Use the [API documentation](https://docs.rs/taskvisor) for their exact contracts.
