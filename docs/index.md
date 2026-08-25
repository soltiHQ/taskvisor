---
title: Taskvisor overview
description: Supervise in-process Tokio work with lifecycle policies, runtime control, direct outcomes, and per-key admission.
---

# Taskvisor overview

Taskvisor is an in-process task supervisor for Rust services.
It turns Tokio work into named tasks with explicit lifecycle policy, cancellation, runtime control, final outcomes, and optional per-key admission.

Taskvisor owns lifecycle mechanics inside one process.
The application still owns task logic, external side effects, durable state, deployment, authentication, authorization, and other security policy.

## What Taskvisor manages

| Need                  | Taskvisor contract                                                                                       |
|-----------------------|----------------------------------------------------------------------------------------------------------|
| Supervised lifecycle  | One-shot, retrying, or periodic tasks with backoff, jitter, retry limits, and attempt timeouts.          |
| Runtime control       | Add work and optionally request its final result; inspect, cancel, or remove registrations later.        |
| Direct final outcomes | One process-local `TaskOutcome` through `TaskWaiter`, outside the best-effort event path.                |
| Per-key admission     | Queue, replace, or reject competing submissions through an optional controller slot.                     |
| Typed observability   | Lifecycle events for logs, traces, metrics, and live diagnostics.                                        |
| Explicit resource use | Configurable bounds for attempts, registrations, command queues, subscriber queues, and retained values. |

## Choose a workflow

| Application need                                       | Taskvisor path                                  | Start here                                                                                        |
|--------------------------------------------------------|-------------------------------------------------|---------------------------------------------------------------------------------------------------|
| A fixed batch or resident workers known at startup     | `run`, `run_until`, or `run_with_os_signals`    | [Run Taskvisor](running-and-managing.md) and [graceful_worker.rs](../examples/graceful_worker.rs) |
| Work discovered while the service is running           | `serve`, then `add*` through `SupervisorHandle` | [Manage tasks at runtime](managing-tasks.md) and [dynamic_tasks.rs](../examples/dynamic_tasks.rs) |
| Competing work that must coordinate by application key | `serve`, then controller `submit*` methods      | [Coordinate work by key](keyed-admission.md) and [tenant_sync.rs](../examples/tenant_sync.rs)     |

These paths describe how work enters the runtime.
`TaskSpec::once`, `restartable`, and `periodic` separately describe what happens after each attempt.
Use a watched add or submission whenever application logic needs the final outcome.

## Check the fit

Taskvisor is intended for services that need one lifecycle and management boundary for concerns such as:

- tasks are added, removed, or watched while a service is running;
- attempts need retry limits, timeouts, backoff, or coordinated cancellation;
- application logic needs the final outcome of one task;
- competing work for the same application key needs an explicit queue, replace, or reject policy.

Taskvisor may be more than an application needs for one retrying future or a small fixed set of workers with simple cancellation.
It is not a persistent job queue or an external scheduler.

## Know the main boundaries

- Tasks, queued submissions, task IDs, events, and watched outcomes do not survive process exit.
- Cancellation is cooperative. Timeouts and force-abort do not roll back external side effects.
- `TaskWaiter` is the direct result path. Events and subscriber queues are bounded and best-effort.
- Periodic work uses a delay after completion, not cron or missed-run recovery.
- Controller slots and resource budgets belong to one supervisor. They do not coordinate another process or bound operating-system resources.

Read [Production boundaries](production-boundaries.md) before deploying a service.

## Start using Taskvisor

- Complete the [Quick start](quick-start.md) for one retrying watched task.
- Read the [Mental model](mental-model.md) to separate entry paths, task behavior, identities, and result paths.
- Browse the [Taskvisor examples](../examples/README.md) for complete static, dynamic, observability, and keyed-admission programs.
- Use the [API documentation](https://docs.rs/taskvisor) for exact signatures, variants, and edge-case contracts.

## All guide pages

- **Start:** [Quick start](quick-start.md), [Mental model](mental-model.md), [Install Taskvisor](installation.md).
- **Use Taskvisor:** [Define a task](defining-tasks.md), [Choose task behavior](lifecycle-policies.md), [Run Taskvisor](running-and-managing.md), [Manage tasks at runtime](managing-tasks.md), [Final outcomes and lifecycle events](outcomes-and-events.md), [Cancellation and shutdown](cancellation-and-shutdown.md), [Coordinate work by key](keyed-admission.md).
- **Operate:** [Configure Taskvisor](configuration.md), [Production boundaries](production-boundaries.md), [Common mistakes](common-mistakes.md).
- **Reference:** [Taskvisor examples](../examples/README.md), [API reference](https://docs.rs/taskvisor).
