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
| `controller`         | Yes.    | Slot-based admission control.                          |
| `tracing`            | No.     | `TracingBridge` for the `tracing` ecosystem.           |
| `logging`            | No.     | `LogWriter` for simple readable lifecycle output.      |
| `tokio-util-interop` | No.     | Access to the raw cancellation token in `TaskContext`. |
| `test-util`          | No.     | Constructors intended for external integration tests.  |

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
Use the [API documentation](https://docs.rs/taskvisor) for their exact contracts.
