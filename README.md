[![Minimum Rust 1.85](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://rust-lang.org)
[![Crates.io](https://img.shields.io/crates/v/taskvisor.svg)](https://crates.io/crates/taskvisor)
[![Apache 2.0](https://img.shields.io/badge/license-Apache2.0-orange.svg)](./LICENSE)

# Taskvisor
> Helps you build resilient, event-driven async systems in Rust.

It's a small orchestrator that watches over your background tasks restarting, tracking, and signaling what happens.
```text
 ┌────────────┐     runs & restarts    ┌──────────────┐
 │   Tasks    │ ◄───────────────────── │  Supervisor  │
 └────────────┘                        └──────┬───────┘
                                              ▼
                                         emits events
                                              ▼
                                       ┌──────────────┐
                                       │ Subscribers  │ ◄─ your listeners
                                       └──────────────┘
```
Use it for long-lived jobs, controllers, or background workers that must stay alive, observable, and restart safely.

<div>
  <a href="https://docs.rs/taskvisor/latest/taskvisor/"><img alt="API Docs" src="https://img.shields.io/badge/API%20Docs-4d76ae?style=for-the-badge&logo=rust&logoColor=white"></a>
  <a href="./examples/"><img alt="Examples" src="https://img.shields.io/badge/Examples-2ea44f?style=for-the-badge&logo=github&logoColor=white"></a>
</div>

## Why Taskvisor?
Async systems grow fast: tasks, loops, background jobs, controllers.    
Eventually you need structure: who runs what, what happens on failure, how to restart safely, and how to observe it all.
Taskvisor provides that structure: a small, event-driven supervision layer that keeps async work supervised and transparent.

## Quick example
Runs forever, restarts automatically on each completion, and emits lifecycle events:
```rust
use tokio_util::sync::CancellationToken;

use taskvisor::{
    Supervisor, TaskFn, TaskSpec, Config, TaskRef,
    TaskError, RestartPolicy, BackoffPolicy,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sup = Supervisor::new(Config::default(), Vec::new());
    
    let ping: TaskRef = TaskFn::arc("ping", |ctx: CancellationToken| async move {
        if ctx.is_cancelled() {
            return Err(TaskError::Canceled);
        }

        println!("[ping] pong");
        Ok(())
    });
    let spec = TaskSpec::new(
        ping,
        RestartPolicy::Always,
        BackoffPolicy::default(),
        None,
    );

    sup.run(vec![spec]).await?;
    Ok(())
}
```

### More Examples
```bash
cargo run --example {{ example_name }} --features {{ features }}
```
| Example                                                 | Description                                         | Features   |
|---------------------------------------------------------|-----------------------------------------------------|------------|
| [retry_with_backoff.rs](examples/retry_with_backoff.rs) | Retry loop with exponential backoff and jitter      | -          |
| [subscriber.rs](examples/subscriber.rs)                 | Custom subscriber reacting to events                | -          |
| [control.rs](examples/control.rs)                       | Tasks add/remove/cancel while supervisor is running | -          |
| [controller.rs](examples/controller.rs)                 | Tasks example with additional admissions            | controller |

## Key features
- **[Supervisor](./src/core/supervisor.rs)** manage async tasks, tracks lifecycle, handles requests, and drives graceful shutdown.
- **[Registry](./src/core/registry.rs)** coordinates task actors, spawning and removing them in response to runtime events.
- **[TaskActor](./src/core/actor.rs)** per-task execution loop applying restart, backoff, and timeout policies for each run.
- **[TaskSpec](./src/tasks/spec.rs)** declarative task definition combining restart behavior, backoff strategy, and optional timeout.
- **[TaskFn](./src/tasks/impl/func.rs)** lightweight adapter turning async closures into supervised tasks.
- **[RestartPolicy](./src/policies/restart.rs)** / **[BackoffPolicy](./src/policies/backoff.rs)** configurable restart control and retry delay strategies.
- **[Bus](./src/events/bus.rs)** broadcast channel delivering structured runtime events to subscribers and the controller.
- **[Event](./src/events/event.rs)** typed event model.
- **[Subscribe](./src/subscribers/subscriber.rs)** extension point for custom event consumers, each with its own bounded queue and worker.
- **[Controller](./src/controller/mod.rs)** *(feature = `controller`)* slot-based orchestrator with admissions: `Queue`, `Replace`, and `DropIfRunning`.

### Runtime diagram
```text
TaskFn (your async code with CancellationToken)
   ├─► wrapped in TaskSpec
   │   (RestartPolicy + BackoffPolicy + timeout)
   └─► passed to Supervisor
           ├─► spawns Registry
           │      └─► creates TaskActor per task
           │             └─► runs TaskSpec in loop
           │                    └─► publishes Event to Bus
           └─► Bus distributes events to:
                  └─► Subscribe implementations (your metrics/logs)

Optional Controller (feature-gated):
   ControllerSpec ─► Controller ─► Supervisor.add_task(TaskSpec)
   (admission policies: Queue/Replace/DropIfRunning)
```

## Optional features
| Feature       | Description                                                             | Use when               |
|---------------|-------------------------------------------------------------------------|------------------------|
| `controller`  | Enables slot-based orchestration (`Controller`, `ControllerSpec`, etc.) | Need admission control |
| `logging`     | Enables the built-in `LogWriter`, _demo_ logger                         | Debugging (Demo)       |

## Contributing
We're open to any new ideas and contributions.  
Found a bug? Have an idea? We welcome pull requests and issues.
