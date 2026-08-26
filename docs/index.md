---
title: Taskvisor overview
description: Supervise in-process Tokio work with lifecycle policies, runtime control, direct outcomes, and per-key admission.
---

# Taskvisor overview

Taskvisor runs and manages Tokio tasks inside a Rust service.
It gives tasks a name, retry rules, cancellation, and a final result.
An optional controller can queue, replace, or reject work for the same application key.

Taskvisor manages the task lifecycle inside one process.
Your application still owns the work itself, external side effects, stored data, deployment, and security.

[crates.io](https://crates.io/crates/taskvisor) · [API reference](https://docs.rs/taskvisor) · [Source code](https://github.com/soltiHQ/taskvisor)

## What Taskvisor manages

| Need                  | Taskvisor contract                                                                                                                                                                                                                           |
|-----------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Supervised lifecycle  | One-shot, retrying, or periodic tasks with backoff, jitter, retry limits, and attempt timeouts.                                                                                                                                              |
| Runtime control       | Add work and optionally request its final result; inspect, cancel, or remove registrations later.                                                                                                                                            |
| Direct final outcomes | One process-local [`TaskOutcome`](https://docs.rs/taskvisor/latest/taskvisor/core/enum.TaskOutcome.html) through [`TaskWaiter`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.TaskWaiter.html), outside the best-effort event path. |
| Per-key admission     | Queue, replace, or reject competing submissions through an optional controller slot.                                                                                                                                                         |
| Typed observability   | Lifecycle events for logs, traces, metrics, and live diagnostics.                                                                                                                                                                            |
| Explicit resource use | Configurable bounds for attempts, registrations, command queues, subscriber queues, and retained values.                                                                                                                                     |

## Choose a workflow

| Application need                                       | Taskvisor path                                                                                                                                                                                                                                                                                                                    | Start here                                                                                        |
|--------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|---------------------------------------------------------------------------------------------------|
| A fixed batch or resident workers known at startup     | [`run`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.run), [`run_until`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.run_until), or [`run_with_os_signals`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.run_with_os_signals) | [Run Taskvisor](running-and-managing.md) and [graceful_worker.rs](../examples/graceful_worker.rs) |
| Work discovered while the service is running           | [`serve`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.serve), then `add*` through [`SupervisorHandle`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.SupervisorHandle.html)                                                                                                            | [Manage tasks at runtime](managing-tasks.md) and [dynamic_tasks.rs](../examples/dynamic_tasks.rs) |
| Competing work that must coordinate by application key | [`serve`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.Supervisor.html#method.serve), then controller `submit*` methods                                                                                                                                                                                                 | [Coordinate work by key](keyed-admission.md) and [tenant_sync.rs](../examples/tenant_sync.rs)     |

These paths describe how work enters the runtime.
[`TaskSpec::once`](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskSpec.html#method.once), [`restartable`](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskSpec.html#method.restartable), and [`periodic`](https://docs.rs/taskvisor/latest/taskvisor/tasks/struct.TaskSpec.html#method.periodic) separately describe what happens after each attempt.
Use a watched add or submission whenever application logic needs the final outcome.

## Check the fit

Taskvisor can help when a service needs to:

- add, remove, or watch tasks while the service is running;
- set retry limits, timeouts, backoff, or shared cancellation;
- wait for the final result of one task;
- queue, replace, or reject competing work for the same key.

Taskvisor may be more than an application needs for one retrying future or a small fixed set of workers with simple cancellation.
It is not a persistent job queue or an external scheduler.

## Know the main boundaries

- Tasks, queued submissions, task IDs, events, and watched outcomes do not survive process exit.
- Cancellation is cooperative. Timeouts and force-abort do not roll back external side effects.
- [`TaskWaiter`](https://docs.rs/taskvisor/latest/taskvisor/core/struct.TaskWaiter.html) is the direct result path. Events and subscriber queues are bounded and best-effort.
- Periodic work uses a delay after completion, not cron or missed-run recovery.
- Controller slots and resource budgets belong to one supervisor. They do not coordinate another process or bound operating-system resources.

Read [Production boundaries](production-boundaries.md) before deploying a service.

## Start using Taskvisor

- Complete the [Quick start](quick-start.md) for one retrying watched task.
- Read the [Mental model](mental-model.md) to separate entry paths, task behavior, identities, and result paths.
- Follow the [source map](../src/ARCHITECTURE.md) to see which module owns each part of the runtime.
- Browse the [Taskvisor examples](../examples/README.md) for complete static, dynamic, observability, and keyed-admission programs.
- Use the [API documentation](https://docs.rs/taskvisor) for exact signatures, variants, and edge-case contracts.

## All guide pages

- **Start:** [Quick start](quick-start.md), [Mental model](mental-model.md), [Install Taskvisor](installation.md).
- **Use Taskvisor:** [Define a task](defining-tasks.md), [Choose task behavior](lifecycle-policies.md), [Run Taskvisor](running-and-managing.md), [Manage tasks at runtime](managing-tasks.md), [Final outcomes and lifecycle events](outcomes-and-events.md), [Cancellation and shutdown](cancellation-and-shutdown.md), [Coordinate work by key](keyed-admission.md).
- **Operate:** [Configure Taskvisor](configuration.md), [Production boundaries](production-boundaries.md), [Common mistakes](common-mistakes.md).
- **Reference:** [Taskvisor examples](../examples/README.md), [API reference](https://docs.rs/taskvisor).
