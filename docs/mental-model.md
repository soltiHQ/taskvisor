---
title: Mental model
description: Understand Taskvisor tasks, specifications, identities, supervision, and observation paths.
---

# Mental model

A task defines executable work. A task specification gives that work a name and lifecycle policy.
The supervisor owns registration, attempts, cancellation, and cleanup.

## Follow work through the runtime

```text
Task / TaskFn ──► TaskSpec
                      ├── add* ──► registry
                      └── ControllerSpec
                               └── submit* ──► controller
                                                    ├── admitted ──► registry
                                                    └── rejected ──► watched TaskWaiter

registry ──► supervised attempts
                 ├── watched ──► TaskWaiter
                 └── observed ─► Event subscribers
```

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

Direct `add*` methods send a `TaskSpec` to the runtime registry.
Controller `submit*` methods first apply a per-slot admission policy, then hand admitted work to the same registry.
