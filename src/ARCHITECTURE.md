# Taskvisor contributor map

This document is the entry point for contributors and reviewers.
It explains what each part of the project owns, how the parts connect, and where to begin a change.

For application usage, start with the [README](../README.md), the [user guide](../docs/index.md), the [crate documentation](https://docs.rs/taskvisor), and the [examples guide](../examples/README.md).
Exact contracts live in the Rust source and its module-level documentation.

## Architecture at a glance

All registry-admitted tasks share the same execution path.
The optional controller adds one admission step before the registry and can reject work without handing it off.

```text
application
├── run* ───► registry
├── serve ──► SupervisorHandle ──► add* ─────► registry
└── serve ──► SupervisorHandle ──► submit* ──► controller ──► registry

registry
├── admitted task ───────────► TaskActor ──► run_once ──► Task
└── membership and removal ──► terminal classification

watched final outcome or watched submission rejection ──► TaskWaiter
runtime components ────► bounded event bus ──► subscriber lanes
terminal user values ──► deferred cleanup domain
```

`run*` means `run`, `run_until`, or `run_with_os_signals`. These methods manage an initial batch.
`serve` returns a `SupervisorHandle` for dynamic management.
The handle exposes `add*`, `submit*`, and `prepare_submission`.
A `PreparedSubmission` exposes its own `submit*` methods for the reserved `TaskId`.

`SupervisorCore` connects the runtime components. The registry owns task membership.
A `TaskActor` owns the lifecycle of one registered task. `run_once` executes one physical attempt.

## Boundaries to preserve

These rules define the main architecture:

- The registry is the source of truth for registered membership, removal, and terminal classification.
- The controller owns pending submission payloads until registry hand-off. The registry then owns the admitted task, while the controller keeps slot coordination until physical release.
- Each registry-admitted task has one actor and keeps its existing `TaskId`. Attempts for that ID never overlap.
- Task names, controller slots, and task IDs have separate roles.
- Management replies and `TaskWaiter` use direct in-process paths. Events are best-effort observability and never drive runtime state.
- Shutdown is one shared operation. Logical force-abort does not prove that a physical attempt has stopped.
- Shutdown deadlines do not guarantee that a subscriber callback or destructor has finished.
- Retained task and subscriber values use the deferred cleanup domain for final destruction.

## Source map

Use this table before following internal calls.

| Area                 | Responsibility                                                        | Start here                                                                                                                 |
|----------------------|-----------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------|
| Crate surface        | Public modules, re-exports, and feature gates                         | [`lib.rs`](lib.rs), [`prelude.rs`](prelude.rs)                                                                             |
| Task model           | `Task`, `TaskFn`, `TaskSpec`, and cancellation context                | [`tasks/mod.rs`](tasks/mod.rs)                                                                                             |
| Retry policy         | Restart, backoff, jitter, and retry timing                            | [`policies/mod.rs`](policies/mod.rs)                                                                                       |
| Runtime package      | Internal composition and public runtime re-exports                    | [`core/mod.rs`](core/mod.rs)                                                                                               |
| Construction         | Runtime limits, task defaults, and component wiring                   | [`core/builder.rs`](core/builder.rs), [`core/config.rs`](core/config.rs), [`core/task_defaults.rs`](core/task_defaults.rs) |
| Public lifecycle     | Static run methods, dynamic handle methods, and public ownership      | [`core/supervisor.rs`](core/supervisor.rs), [`core/handle.rs`](core/handle.rs), [`core/owner.rs`](core/owner.rs)           |
| Runtime coordination | Startup, management routing, event relay, and shared shutdown         | [`core/runtime/mod.rs`](core/runtime/mod.rs)                                                                               |
| Registry             | Authoritative admission, membership, scheduling, removal, and cleanup | [`core/registry/mod.rs`](core/registry/mod.rs)                                                                             |
| Task execution       | Restart loop and one physical attempt                                 | [`core/actor.rs`](core/actor.rs), [`core/runner.rs`](core/runner.rs)                                                       |
| Watched results      | Final `TaskOutcome` delivery                                          | [`core/outcome.rs`](core/outcome.rs)                                                                                       |
| Deferred cleanup     | Ownership capacity, public snapshots, and off-runtime destruction      | [`core/ownership.rs`](core/ownership.rs), [`core/deferred_drop/mod.rs`](core/deferred_drop/mod.rs)                         |
| Controller API       | Slots, policies, configuration, submissions, snapshots, and errors    | [`controller/mod.rs`](controller/mod.rs)                                                                                   |
| Controller engine    | Ordered commands, slot state, admission, identity, and shutdown       | [`controller/engine/mod.rs`](controller/engine/mod.rs)                                                                     |
| Event model          | Event values and bounded ingress                                      | [`events/mod.rs`](events/mod.rs)                                                                                           |
| Event delivery       | Per-subscriber queues, callback lanes, and built-in observers         | [`subscribers/mod.rs`](subscribers/mod.rs), [`core/runtime/event_relay.rs`](core/runtime/event_relay.rs)                   |
| Shared contracts     | Public errors, identities, and internal diagnostic text               | [`error.rs`](error.rs), [`identity.rs`](identity.rs), [`reasons.rs`](reasons.rs)                                           |

Files outside `src/` provide executable context:

| Path                              | Purpose                                                 |
|-----------------------------------|---------------------------------------------------------|
| [`examples/`](../examples)        | Runnable public workflows and expected behavior         |
| [`tests/`](../tests)              | Cross-component contracts and regressions               |
| [`benches/`](../benches)          | Measured performance boundaries                         |
| [`Cargo.toml`](../Cargo.toml)     | Features, dependencies, examples, and benchmark targets |
| [`Taskfile.yml`](../Taskfile.yml) | Repository validation commands                          |

## Follow the main flows

### 1. Build and start

`SupervisorBuilder` combines `SupervisorConfig`, `TaskDefaults`, subscribers, the cleanup ownership domain, and an optional controller.
It returns a `Supervisor` around the shared runtime core.

`Supervisor::run`, `Supervisor::run_until`, and `Supervisor::run_with_os_signals` supply an initial task batch.
`Supervisor::serve` starts the same runtime and returns a `SupervisorHandle`.
Startup and direct runtime management enter through [`core/runtime/`](core/runtime).

### 2. Register and run a task

Direct adds and static batches reach the registry. A controller submission reaches the same registry after slot admission.

```text
TaskSpec ────────► registry admission ──► TaskActor
TaskActor ───────► run_once ────────────► attempt future
attempt result ──► actor policy ────────► stop, retry, or repeat
actor exit ──────► registry removal ────► optional watched TaskOutcome
```

Registry admission resolves inherited `TaskDefaults` and indexes the task ID and name.
The actor owns restart policy, retry counting, and delays between attempts.
The runner owns one attempt, including timeout and panic capture.

When the actor ends, registry removal owns terminal classification and watched outcome delivery.
Module docs under [`core/registry/`](core/registry) describe the admission and removal protocols.

### 3. Coordinate work through the controller

The `controller` feature is enabled by default, but controller admission is a runtime opt-in through `SupervisorBuilder::with_controller`.
Direct `add*` methods bypass it. `SupervisorHandle::submit*` methods use it.

```text
ControllerSpec ──► controller slot
                         ├── idle ──► registry admission
                         └── busy ──► queue, replace, or reject
```

| Value           | Role                                                                                                         |
|-----------------|--------------------------------------------------------------------------------------------------------------|
| `TaskId`        | Identity allocated for one task request, including a prepared request; it does not prove intake or admission |
| Task name       | Uniqueness key within one supervisor registry and diagnostic label                                           |
| Controller slot | Application key that serializes competing work                                                               |

The controller owns ordered commands and pending payloads. Registry decisions and physical completion signals drive slot transitions.
Events do not. Start with [`controller/mod.rs`](controller/mod.rs) for the public contract and [`controller/engine/mod.rs`](controller/engine/mod.rs) for implementation.

With a controller configured, cancel and remove operations by `TaskId` pass through the controller before the registry.
This lets them reach queued submissions and preserves controller command order.
`cancel_by_name` and `remove_by_name` target registered work because queued submissions do not own a registered name.

### 4. Return results or publish observations

Choose the source that answers the question:

| Question                                                       | Source                                               |
|----------------------------------------------------------------|------------------------------------------------------|
| Did a management command commit or claim the requested action? | The method's direct return value                     |
| How did watched work end?                                      | `TaskWaiter` through a direct in-process channel     |
| Is a task registered or being removed?                         | `SupervisorHandle::list`                             |
| Does a task still have a physical attempt?                     | `alive_snapshot` or `is_alive`                       |
| What is the current controller view?                           | `controller_snapshot`, a rolling diagnostic snapshot |
| What happened for logs, metrics, or tracing?                   | Best-effort events and subscribers                   |

A management reply is not a final task result. A `TaskWaiter` is direct but not durable across process termination.
Event loss does not change runtime state or watched outcomes. A controller `submit*` reply confirms command intake,
not positive slot admission.

### 5. Shut down and release ownership

Several triggers join the same shutdown operation:

```text
shutdown trigger
├── explicit handle request ────────────────────► shared coordinator
├── run_until future ───────────────────────────► shared coordinator
├── Taskvisor-owned OS listener ────────────────► shared coordinator
└── registry becomes empty during static run* ──► shared coordinator

shared coordinator ──► close intake ─────────────────► finish commands already accepted
                   ──► drain registry within grace ──► stop runtime workers
```

`run_with_os_signals` is the only entry point that installs Taskvisor's operating-system signal listeners.
The shared coordinator caches one result for all callers.

The grace verdict reports whether registry actors and pending removals drained in time.
It does not promise that every physical callback or destructor has finished.
Taskvisor may keep owning a force-aborted actor until its physical attempt exits.
Remaining retained task and subscriber values move to [`core/deferred_drop/`](core/deferred_drop), where blocking or panicking destructors are isolated from runtime paths.

## Find the code for a change

| Change                                           | Start here                                                                                                                                                                                                                                                        | Verify with                                                                                                                           |
|--------------------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------|
| Public exports, features, errors, or identity    | [`Cargo.toml`](../Cargo.toml), [`lib.rs`](lib.rs), [`prelude.rs`](prelude.rs), [`error.rs`](error.rs), [`identity.rs`](identity.rs)                                                                                                                               | Crate docs and affected examples                                                                                                      |
| Task contract, cancellation, or task settings    | [`tasks/`](tasks), [`core/task_defaults.rs`](core/task_defaults.rs)                                                                                                                                                                                               | [`tests/defaults.rs`](../tests/defaults.rs), relevant examples                                                                        |
| Restart, retry, backoff, or delay behavior       | [`policies/`](policies), [`core/actor.rs`](core/actor.rs)                                                                                                                                                                                                         | [`tests/failure.rs`](../tests/failure.rs), [`tests/lifecycle.rs`](../tests/lifecycle.rs)                                              |
| One attempt, timeout, or panic capture           | [`core/runner.rs`](core/runner.rs)                                                                                                                                                                                                                                | [`tests/timeout.rs`](../tests/timeout.rs), [`tests/failure.rs`](../tests/failure.rs)                                                  |
| Static or dynamic runtime API                    | [`core/supervisor.rs`](core/supervisor.rs), [`core/handle.rs`](core/handle.rs), [`core/runtime/lifecycle/static_run.rs`](core/runtime/lifecycle/static_run.rs), [`core/runtime/management/`](core/runtime/management)                                             | [`tests/lifecycle.rs`](../tests/lifecycle.rs), [`tests/concurrency.rs`](../tests/concurrency.rs)                                      |
| Admission, names, removal, or actor ownership    | [`core/registry/`](core/registry)                                                                                                                                                                                                                                 | [`tests/identity.rs`](../tests/identity.rs), [`tests/watch.rs`](../tests/watch.rs), [`tests/concurrency.rs`](../tests/concurrency.rs) |
| Watched outcomes                                 | [`core/outcome.rs`](core/outcome.rs), [`core/registry/removal/`](core/registry/removal)                                                                                                                                                                           | [`tests/watch.rs`](../tests/watch.rs)                                                                                                 |
| Events or subscriber delivery                    | [`events/`](events), [`subscribers/`](subscribers), [`core/runtime/event_relay.rs`](core/runtime/event_relay.rs)                                                                                                                                                  | [`tests/lifecycle.rs`](../tests/lifecycle.rs), module unit tests                                                                      |
| Controller public behavior                       | Public files in [`controller/`](controller)                                                                                                                                                                                                                       | [`tests/controller.rs`](../tests/controller.rs)                                                                                       |
| Controller queue, replace, reject, or slot state | [`controller/engine/admission/`](controller/engine/admission), [`controller/engine/state/`](controller/engine/state)                                                                                                                                              | Controller engine unit tests, [`tests/controller.rs`](../tests/controller.rs)                                                         |
| Shared shutdown order or grace behavior          | [`core/runtime/shutdown_workflow/`](core/runtime/shutdown_workflow), [`core/runtime/lifecycle/`](core/runtime/lifecycle), [`core/registry/removal/`](core/registry/removal), [`controller/engine/lifecycle/shutdown.rs`](controller/engine/lifecycle/shutdown.rs) | [`tests/shutdown.rs`](../tests/shutdown.rs), [`tests/ownership.rs`](../tests/ownership.rs)                                            |
| Operating-system signal handling                 | [`core/shutdown.rs`](core/shutdown.rs), [`core/supervisor.rs`](core/supervisor.rs)                                                                                                                                                                                | [`tests/signal_ownership.rs`](../tests/signal_ownership.rs)                                                                           |
| Ownership limits or deferred cleanup             | [`core/config.rs`](core/config.rs), [`core/builder.rs`](core/builder.rs), [`core/ownership.rs`](core/ownership.rs), [`core/deferred_drop/`](core/deferred_drop), [`core/registry/removal/`](core/registry/removal)                                                    | [`tests/ownership.rs`](../tests/ownership.rs), [`tests/shutdown.rs`](../tests/shutdown.rs)                                            |
| User-facing documentation or workflows           | [`README.md`](../README.md), [`docs/`](../docs/index.md), [`examples/`](../examples), [`lib.rs`](lib.rs)                                                                                                                                                          | Example compilation and crate docs                                                                                                    |

## Read and validate a change

1. Start with this map and the module-level docs for the affected area.
2. Read the public entry point and the component that owns the decision.
3. Read the matching integration test before following deeper internal modules.
4. Keep cross-component behavior in `tests/` and local state-machine cases beside their module.
5. Use `examples/` to verify the application-facing story. Use `benches/` only for measured performance boundaries.

The repository tasks for formatting, checking, linting, tests, and docs are listed in the [contributing section](../README.md#contributing)
and implemented in [`Taskfile.yml`](../Taskfile.yml), [source](https://taskfile.dev/).
