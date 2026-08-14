# Taskvisor examples

Each file is a complete runnable program. 
Run every command from the repository root.
Start with `basic`, then choose the branch closest to your application.

## Choose a path

Arrows show a suggested reading order, not runtime data flow.

```text
basic
├── reusable task type ───► task_type
├── resident lifecycle ───► graceful_worker ──► application_shutdown
├── policies and limits ──► periodic ─────────► restart_policies ──► configuration
├── results and control ──► outcomes ─────────► dynamic_tasks
├── service patterns ─────► queue_consumer ───► cpu_job
├── observability ────────► custom_subscriber
│                              ├── readable logs ────────► logging
│                              ├── structured events ────► tracing
│                              └── Prometheus counters ──► metrics
└── keyed admission ──────► controller_slots ──► controller_admission ──► tenant_sync
```

## Catalog

Only examples marked `Ctrl+C` wait for a signal. 
The others exit on their own. 
Commands use default features unless they include an explicit `--features` flag. 
The default `controller` feature covers the keyed-admission examples.

| Example                                            | What to learn                                                     | Run                                              | Stop   |
|----------------------------------------------------|-------------------------------------------------------------------|--------------------------------------------------|--------|
| [basic.rs](basic.rs)                               | Run one static task once.                                         | `cargo run --example basic`                      | —      |
| [task_type.rs](task_type.rs)                       | Implement `Task` for reusable state across fresh attempts.        | `cargo run --example task_type`                  | —      |
| [graceful_worker.rs](graceful_worker.rs)           | Make a resident worker observe cooperative cancellation.          | `cargo run --example graceful_worker`            | Ctrl+C |
| [application_shutdown.rs](application_shutdown.rs) | Stop a static batch with an application-owned future.             | `cargo run --example application_shutdown`       | —      |
| [periodic.rs](periodic.rs)                         | Repeat short work with fixed-delay scheduling.                    | `cargo run --example periodic`                   | Ctrl+C |
| [restart_policies.rs](restart_policies.rs)         | Combine one-shot, retrying, and periodic tasks.                   | `cargo run --example restart_policies`           | Ctrl+C |
| [configuration.rs](configuration.rs)               | Set supervisor limits, task defaults, and per-task overrides.     | `cargo run --example configuration`              | —      |
| [outcomes.rs](outcomes.rs)                         | Await completed, failed, fatal, timed-out, and canceled outcomes. | `cargo run --example outcomes`                   | —      |
| [dynamic_tasks.rs](dynamic_tasks.rs)               | Add, inspect, cancel, and remove tasks at runtime.                | `cargo run --example dynamic_tasks`              | —      |
| [queue_consumer.rs](queue_consumer.rs)             | Retry a broker session and make receive cancellation-aware.       | `cargo run --example queue_consumer`             | —      |
| [cpu_job.rs](cpu_job.rs)                           | Move CPU work to Rayon and understand its cancellation limit.     | `cargo run --example cpu_job`                    | —      |
| [custom_subscriber.rs](custom_subscriber.rs)       | Handle typed events through a bounded best-effort subscriber.     | `cargo run --example custom_subscriber`          | —      |
| [logging.rs](logging.rs)                           | Print readable lifecycle events with `LogWriter`.                 | `cargo run --example logging --features logging` | —      |
| [tracing.rs](tracing.rs)                           | Send structured lifecycle events to `tracing`.                    | `cargo run --example tracing --features tracing` | —      |
| [metrics.rs](metrics.rs)                           | Build Prometheus counters from stable event labels.               | `cargo run --example metrics`                    | —      |
| [controller_slots.rs](controller_slots.rs)         | Compare queue, replace, and reject policies by slot.              | `cargo run --example controller_slots`           | —      |
| [controller_admission.rs](controller_admission.rs) | Observe typed admission outcomes and a controller snapshot.       | `cargo run --example controller_admission`       | —      |
| [tenant_sync.rs](tenant_sync.rs)                   | Keep only the newest waiting revision for each tenant.            | `cargo run --example tenant_sync`                | —      |
