# Taskvisor benchmarks

These benchmarks show how fast Taskvisor completes specific operations on the machine that runs them.
Each result states what was timed and what stayed outside the timer.

## Run the benchmarks

Run all five benchmark suites with the colored Taskvisor report:

```bash
task rust:test/bench
```

The equivalent Cargo command is:

```bash
cargo bench --bench '*' --features controller --locked -- --quiet --color always
```

Run each selected case as a smoke check, without collecting statistical estimates:

```bash
cargo bench --bench '*' --features controller --locked -- --test
```

Smoke mode exercises the case assertions. It does not produce the Taskvisor performance snapshot.

Run one suite:

```bash
cargo bench --bench controller --features controller --locked -- --color always
```

Run matching cases from one suite:

```bash
cargo bench --bench controller --features controller --locked -- 'controller/cold/first_try_submit'
```

The shared defaults are 30 samples, 1 second of warmup, and 3 seconds of measurement per case.
Criterion flags such as `--sample-size`, `--warm-up-time`, and `--measurement-time` override these defaults.
Setup, checks, and cleanup can make the full command take longer than the measurement window.

## What each case measures

### [Lifecycle](lifecycle.rs)

| Case                       | Timed work                                                                                                                                                | What it does not measure                                                                                          |
|----------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------------|
| Cold single task           | Fresh supervisor construction, one instant task, and shared shutdown. The task body is checked to have run.                                               | Tokio runtime and task-value construction.                                                                        |
| Watched completion         | Admission through `Completed`, either on the first attempt or after two failed attempts and a successful third attempt.                                   | Startup, task-value construction, deferred ownership cleanup, or a positive backoff delay. Retry backoff is zero. |
| Cancel scheduled backoff   | Cancellation through the reliable `Canceled` outcome after a subscriber has confirmed `BackoffScheduled`.                                                 | Scheduling and waiting out the configured 60-second backoff. The case cancels that wait.                          |
| Finite periodic / `Always` | Admission through exactly eight attempts and a terminal `Canceled` outcome, using `TaskSpec::periodic` or explicit `RestartPolicy::Always`.                 | Startup, task construction, ownership reset, and shutdown.                                                        |
| Cooperative shutdown       | One requested shutdown through shared cleanup with zero or 32 already-started cooperative tasks.                                                          | Startup, admission, start handshakes, and outcome checks.                                                          |
| Grace exceeded             | One requested shutdown through `GraceExceeded` and force-abort commitment with one or 32 already-started non-cooperative tasks under one shared 10ms grace. | Startup, admission, start handshakes, and outcome checks.                                                          |

### [Throughput](throughput.rs)

Every case uses a warmed supervisor without subscribers and requires every watched task to return `Completed`.
The suite separates admission-through-completion measurements from drain-only measurements whose tasks are admitted and whose case-specific initial readiness is observed before timing.

| Case | Timed work | What it does not measure |
|------|------------|--------------------------|
| Instant / one yield / deadline | First watched admission through 256 `Completed` outcomes. The yielding pair differs only by a 60-second attempt deadline that is polled but never expires. | Startup, task construction, ownership reset, shutdown, application I/O, or deadline expiry. |
| `max_concurrent` enabled-path overhead | First watched admission through 256 instant-task outcomes with the limit disabled or set to 1, 4, or 256. | A known saturated state. Instant tasks can finish while the batch is still being admitted. |
| Cooperative CPU drain | One shared release through 64 outcomes after all 64 admitted task bodies have reached the gate. Each body runs 16 deterministic CPU chunks of 4,096 steps and yields between chunks. | Admission, entry handshake, result validation, ownership reset, or application-specific work. This is a synthetic matched runtime workload. |
| Saturated `max_concurrent` drain | One shared release through 64 outcomes with limits 1, 4, or 64. Before timing, the task-body counter and `alive_snapshot` must both remain exactly `min(limit, 64)`; the remaining bodies enter during the timed drain, and all 64 must have entered afterward. | Admission, the pre-release entry and stability handshake, and any claim about an unobserved internal semaphore state. |

The instant-task case is the no-subscriber reference. The fan-out suite repeats that reference as its zero-subscriber variant so subscriber-count comparisons stay in one benchmark family and binary.
The one-yield deadline comparison includes timer registration and removal, deadline selection, and scheduling; it is not a pure timer microbenchmark.
Positive delays and deadline expiry are exercised in the [lifecycle tests](../tests/lifecycle.rs) and [timeout tests](../tests/timeout.rs).

For both drain families, task construction, watched admission, initial observable readiness, outcome-vector allocation, and an explicit first poll that registers the watchdog timer happen before `Instant::now`.
In the saturated family, only the initial `min(limit, 64)` task-body entries are part of that readiness state; entry of the remaining admitted tasks is timed.
The timer starts immediately before the shared release and stops after every watched outcome is received.
Outcome and CPU-result assertions run afterward.
The cooperative CPU family is matched across `current_thread` and the four-worker `multi_thread` runtime; compare those two variants to observe the runtime effect on the same released work.

### [Subscriber fan-out](fanout.rs)

- **Matched shared delivery:** 256 tasks with 0, 1, 4, or 8 short-callback subscribers using the default `SubscriberExecution::Shared` worker. Timing includes watched task completion and every configured subscriber receiving all `TaskFinished` events with `Completed` outcomes. The zero-subscriber case keeps the event bus disabled. Overflow is not allowed.
- **Dedicated short callbacks:** 256 tasks with 1 or 8 short-callback subscribers, each using `SubscriberExecution::Dedicated`. This separate family exposes the native-thread delivery cost without mixing it into the default shared-worker curve.
- **Saturated subscriber:** 256 tasks, one blocked `Dedicated` callback whose queue capacity is one, and 1, 3, or 7 healthy `Shared` subscribers. Timing includes all watched `Completed` outcomes and delivery of their `TaskFinished` events to every healthy subscriber. The blocked callback is confirmed before timing; gate release and verification of its overflow happen afterward.

These rates count completed tasks, not callbacks. The saturated case exercises overflow in one dedicated subscriber lane while tasks and multiple shared healthy subscriber lanes progress.
The zero-to-one delta includes enabling the event bus, the relay, one native callback worker, and cross-thread runtime wake latency; it is not a callback CPU microbenchmark.
Use the matched one-to-four-to-eight `Shared` cases to evaluate subscriber-count scaling.
It does not measure shared event-bus overflow, delivery to the blocked subscriber, or its recovery time.
Setup, final shutdown, and ownership reset are excluded.

### [Dynamic management](dynamic.rs)

| Case                                     | Timed work                                                                                                                    | State prepared or reset outside the timer                                                                                                                              |
|------------------------------------------|-------------------------------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Sequential registry add · root caller    | 32 serialized `add` calls from the `Runtime::block_on` root through their authoritative registry decisions.                    | Task construction, result validation, cancellation, and cleanup of the accepted batch. This is a caller-topology diagnostic, not task completion throughput.           |
| Sequential registry add · spawned caller | 32 serialized `add` calls from one spawned Tokio task through their authoritative registry decisions.                         | Task construction, spawned-task scheduling before the internal timer, root-side `JoinHandle` polling, result validation, cancellation, and cleanup.                       |
| Pipelined registry add · spawned caller  | Concurrent polling of 32 prebuilt `add` futures from one spawned Tokio task through all authoritative registry decisions.     | Task and add-future construction, spawned-task scheduling before the internal timer, root-side `JoinHandle` polling, result validation, cancellation, and cleanup.        |
| Cancel running task                      | Cancellation and the `Canceled` outcome of a task already known to be running.                                                | Admission, the start handshake, and ownership cleanup.                                                                                                                 |
| List held tasks                          | One registry snapshot with 32 or 256 tasks retained.                                                                          | Population, snapshot validation and disposal, and task removal. The unit is a snapshot, not a task.                                                                    |
| Ownership release to admission           | With ownership capacity one, release a gated final task destructor and wait for an already-parked admission to succeed.       | Filling capacity, parking the waiter, completion of the new task, and cleanup. This measures recovery after release, not an arbitrary time spent waiting for capacity. |

The root-caller sequential case is deliberately topology-specific: on the multi-thread runtime, the caller is outside the worker pool while the registry listener runs in it.
Its ratio across runtime variants can depend on runtime scheduling and the host, so use it as a topology diagnostic rather than a portable estimate of dynamic-admission throughput.
The spawned-caller sequential case moves the same serialized request/reply loop into a Tokio task.
The pipelined case keeps the spawned-caller topology but polls all 32 admissions together, making it the batch-throughput comparison.

### [Controller admission](controller.rs)

| Case                  | Timed work                                                                                                              | What it does not measure                                                                           |
|-----------------------|-------------------------------------------------------------------------------------------------------------------------|----------------------------------------------------------------------------------------------------|
| Cold first submission | First successful `try_submit` on a fresh served supervisor, including lazy cleanup-worker startup.                      | Supervisor/controller and Tokio runtime startup, request construction, task outcome, and shutdown. |
| Reused intake burst   | Acceptance of a fixed burst of 64 requests from one caller.                                                             | Slot admission and final task outcomes.                                                            |
| Concurrent producers  | Start-condvar release through exactly 1024 `try_submit` calls and completion-condvar observation by 1, 2, 4, or 8 persistent native callers while the current-thread controller is parked. | Thread spawn/join, batch construction and transfer, readiness wait, acceptance checks, all controller processing, reset, and shutdown. |
| Busy-slot Drop        | 32 requests rejected with `SlotBusy` while an owner holds the slot.                                                     | Owner setup and cleanup.                                                                           |
| Busy-slot Replace     | 32 watched submissions through 31 `SupersededByReplace` rejections, after the newest request replaces the pending head. | The retention snapshot check, newest task completion, and owner teardown.                          |
| One-slot Queue        | 32 task completions through one serial slot, on Tokio current-thread only.                                              | Runtime startup, warmup, request construction, and ownership reset.                                |
| Eight-slot Queue      | 64 task completions across eight slots, on both runtime variants.                                                       | Runtime startup, warmup, request construction, and ownership reset.                                |

In the reused intake burst, the current-thread runtime cannot consume commands during the synchronous producer loop.
The multi-thread runtime can consume them concurrently. Both cases measure submission intake, not completed work.
The concurrent-producer family keeps total work fixed at 1024 and changes only the number of native caller threads.
Those threads are spawned once, receive each prebuilt batch, and park at an observable start line before timing begins.
This family uses the current-thread runtime, whose `block_on` root synchronously waits on the completion condvar.
The controller therefore cannot consume its commands until the timer has stopped.
The timer includes releasing that start condvar, all 1024 `try_submit` calls, and observing all callers on the completion condvar.
It excludes thread spawn/join and per-iteration request construction, dispatch, and readiness synchronization.
The controller queue and ownership capacities both equal 1024, every call must be accepted, and the queued commands, slots, and ownership are drained before the next iteration.
The one-producer result includes the same start/completion synchronization as the multi-producer variants and is their matched reference.

## Measurement boundaries and reset

`cold` uses a fresh supervisor; `Boundary` states whether its construction is timed. `steady` and `reused` exclude startup and warmup.
Each iteration uses a fixed operation or batch. Gates establish required running, busy, or blocked states before timing starts.

A watched outcome can arrive before deferred destruction releases ownership. The fixtures wait for ownership and cleanup to return
to the case's starting level before reusing state. This reset is outside the timer unless `Boundary` explicitly includes it.
Criterion's iteration count therefore does not turn a fixed burst into a growing queue or change the available capacity between iterations.

Shutdown remains outside unrelated steady throughput timers. It is timed only by the cold `Supervisor::run` case and the dedicated requested-shutdown families described above.

The shared fixtures live in [support/fixtures.rs](support/fixtures.rs); the report wrapper stays in [support/mod.rs](support/mod.rs).
`current_thread` uses Tokio's current-thread runtime. `multi_thread` uses four Tokio workers.
These labels describe Tokio only: Taskvisor may also use cleanup workers and subscriber threads.

## How to read a result

The Taskvisor snapshot translates Criterion output into named operations:

Results with the same measured boundary share one card.
Runtime and case-parameter variants appear as separate entries inside that card.

- `Results` is the number of individual runtime and case-parameter measurements.
- `Groups` is the number of shared cards in the report.

- `completed tasks/s` is the number of complete task lifecycles per second.
- `amortized per completed task` is the batch time divided by the number of tasks.
- `for the complete batch` is the measured wall time for the whole batch.
- `95% CI` is the confidence interval reported by Criterion.
- `Boundary` says where timing starts and ends.
- `Outside` lists work that was not timed.
- `Scope` names the semantic unit measured by the card.

Read `Boundary`, `Outside`, and `Scope` before comparing two cases. An intake result measures accepted calls, not completed tasks.
A policy result measures verified controller decisions. A query result measures snapshot calls.

For a batch, the report shows both total time and average time per item.
The average is not the latency of one task inside a concurrent batch.

Criterion prints the generic unit `elem/s`. 
The Taskvisor snapshot replaces it with the real unit, such as completed tasks, accepted submissions, rejections, or snapshot calls.

## Scope labels

The report classifies results by what their rate counts:

| Scope label                                     | Meaning                                                          |
|-------------------------------------------------|------------------------------------------------------------------|
| `COMPLETE MANAGED-TASK LIFECYCLE`               | Each unit is one Taskvisor-managed task completed end to end.    |
| `COMPLETE LIFECYCLE · <NAMED UNIT>`             | The label states the completed unit, such as management cycles.  |
| `TASK DRAIN, NOT END-TO-END LIFECYCLE · <UNIT>` | Admission and the case-specific initial readiness precede the timed release-to-outcome drain. |
| `OPERATION RATE, NOT COMPLETED-TASK THROUGHPUT` | The case measures intake, policy decisions, or query calls.      |

These labels describe the measured unit. They do not rate the result as high or low.
Card colors distinguish measurement scopes. They do not grade performance.

## Compare runs

`Performance has regressed`, `improved`, and `no change` compare the result with a saved Criterion baseline. 
They do not change the absolute time or operation rate from the current run.

Changed measurement boundaries use new case IDs. The earlier cold-batch, CPU-loop, and growing-admission cases are not comparable
with the warmed, fixed-work cases described here. Keep separate baselines for them; the new results do not establish an improvement or regression against those older measurements.

For a separate set of results and HTML reports, set `CRITERION_HOME` when running Cargo:

```bash
CRITERION_HOME=target/criterion-reworked cargo bench --bench '*' --features controller --locked -- --color always
```

Compare matching case IDs, boundaries, parameters, features, and runtime settings on the same machine under comparable load.

## What the result means

A result describes the exact benchmark case on the machine that ran it.
It is useful for comparing repeated runs of the same case and examining one named boundary.

It does not prove how much traffic an application can handle.
Real task work, contention, subscribers, cancellation, memory use, and safety headroom belong in an application load test.
