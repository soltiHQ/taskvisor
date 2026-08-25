---
title: Mental model
description: Understand Taskvisor tasks, specifications, identities, supervision, and observation paths.
---

# Mental model

A task creates executable work for one attempt.
A task specification gives that work a name and lifecycle policy.
One registration gets a task ID and runs its attempts sequentially under the supervisor.

## Choose how work enters

| Entry path                    | Use it for                                                   |
|-------------------------------|--------------------------------------------------------------|
| `Supervisor::run*`            | A fixed batch or resident workers known at startup.          |
| `Supervisor::serve` + `add*`  | Work discovered or managed while the service is running.     |
| `Supervisor::serve` + `submit*` | Work that first needs per-key queue, replace, or reject policy. |

Static batches and direct adds enter the same registry.
Controller submissions apply slot admission first, then hand admitted work to that registry.

## Follow one registration through the runtime

```text
Task / TaskFn ──► TaskSpec
                      ├── run* ──────────────────────► registry
                      ├── add* ──────────────────────► registry
                      └── ControllerSpec ──► submit* ──► controller
                                                              ├── admitted ──► registry
                                                              └── rejected ──► watched TaskWaiter

registry ──► task actor ──► sequential attempts ──► final outcome
                                                     └── watched ──► TaskWaiter
runtime lifecycle ───────────────────────────────────────────────► Event subscribers
```

`TaskSpec::once`, `restartable`, and `periodic` choose behavior after an attempt.
They do not choose a different runtime or executor.
Attempts for one task ID never overlap.

## Know each identity and result path

| Value              | Role                                                                |
|--------------------|---------------------------------------------------------------------|
| `Task` or `TaskFn` | Creates a fresh future for each attempt.                            |
| `TaskSpec`         | Gives work its registry name and execution policy.                  |
| `Supervisor`       | Owns one Taskvisor runtime.                                         |
| `SupervisorHandle` | Manages a running supervisor.                                       |
| `TaskId`           | Identifies one process-local registration or controller submission. |
| Task name          | Uniquely identifies registry membership inside one supervisor.      |
| Controller slot    | Coordinates submissions that must not own the same key together.    |
| `TaskWaiter`       | Delivers one direct in-process final outcome.                       |
| `Event`            | Describes lifecycle activity through best-effort delivery.          |

Task ID, task name, and controller slot answer different questions.
A task ID follows one registration or controller submission.
A task name identifies registry membership.
A controller slot groups submissions that must not own the same application key together.

## Separate command, result, and observation paths

- A management method reports whether its own operation reached the documented boundary.
- A `TaskWaiter` reports how one watched task or submission finally ended.
- Events report lifecycle activity for logs, traces, metrics, and diagnostics.
- `list`, `alive_snapshot`, `ownership_snapshot`, and `controller_snapshot` report different point-in-time runtime views.

An accepted command is not a final task result, and an event is not a reliable command reply.

## Separate logical outcome from physical release

A final outcome ends the watched logical lifecycle.
It does not always mean that every synchronous poll, callback, or user-value destructor has finished.
`ForceAborted` can precede physical attempt exit, and deferred cleanup can retain user values after task membership ends.
Read [Production boundaries](production-boundaries.md) before relying on shutdown deadlines or snapshot state.
