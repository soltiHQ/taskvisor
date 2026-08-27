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

These cases admit and wait for batches of 256 successful tasks on a warmed supervisor, without subscribers.
Each task must return `Completed`. Startup, task-value construction, and ownership cleanup stay outside the timer.
There is no synthetic CPU loop in the task body.

The instant-task case is the no-subscriber reference. The fan-out suite repeats that same reference as its zero-subscriber variant so subscriber-count comparisons stay in one benchmark family and binary.
A second pair uses identical tasks that explicitly yield once, with and without a 60-second attempt deadline.
The yield ensures the deadline timer is polled before the task succeeds.
This comparison includes timer registration and removal, deadline selection, and scheduling; it is not a pure timer microbenchmark.
It does not promise that another task runs during the yield, and it does not measure timeout expiry.
Positive delays and deadline expiry are exercised in the [lifecycle tests](../tests/lifecycle.rs)
and [timeout tests](../tests/timeout.rs); these benchmarks do not measure their elapsed latency.

The `max_concurrent` family runs the same 256 instant tasks with the limit disabled and with limits of 1, 4, and 256.
It times watched admission through every `Completed` outcome, including semaphore acquisition, cancellation selection, permit release, and any configured contention.

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

| Case                           | Timed work                                                                                                              | State prepared or reset outside the timer                                                                                                                              |
|--------------------------------|-------------------------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Bounded registry admission     | 32 `add` calls through registry acceptance, with capacity available.                                                    | Task construction, cancellation, and cleanup of the accepted batch. This is not task completion throughput.                                                            |
| Cancel running task            | Cancellation and the `Canceled` outcome of a task already known to be running.                                          | Admission, the start handshake, and ownership cleanup.                                                                                                                 |
| List held tasks                | One registry snapshot with 32 or 256 tasks retained.                                                                    | Population, snapshot validation and disposal, and task removal. The unit is a snapshot, not a task.                                                                    |
| Ownership release to admission | With ownership capacity one, release a gated final task destructor and wait for an already-parked admission to succeed. | Filling capacity, parking the waiter, completion of the new task, and cleanup. This measures recovery after release, not an arbitrary time spent waiting for capacity. |

### [Controller admission](controller.rs)

| Case                  | Timed work                                                                                                              | What it does not measure                                                                           |
|-----------------------|-------------------------------------------------------------------------------------------------------------------------|----------------------------------------------------------------------------------------------------|
| Cold first submission | First successful `try_submit` on a fresh served supervisor, including lazy cleanup-worker startup.                      | Supervisor/controller and Tokio runtime startup, request construction, task outcome, and shutdown. |
| Reused intake burst   | Acceptance of a fixed burst of 64 requests from one caller.                                                             | Slot admission and final task outcomes.                                                            |
| Concurrent producers  | Synchronized release of 1, 2, 4, or 8 producer tasks through 64 `try_submit` acceptances on Tokio multi-thread.         | Producer creation/readiness, controller decisions and outcomes, reset, and shutdown.               |
| Busy-slot Drop        | 32 requests rejected with `SlotBusy` while an owner holds the slot.                                                     | Owner setup and cleanup.                                                                           |
| Busy-slot Replace     | 32 watched submissions through 31 `SupersededByReplace` rejections, after the newest request replaces the pending head. | The retention snapshot check, newest task completion, and owner teardown.                          |
| One-slot Queue        | 32 task completions through one serial slot, on Tokio current-thread only.                                              | Runtime startup, warmup, request construction, and ownership reset.                                |
| Eight-slot Queue      | 64 task completions across eight slots, on both runtime variants.                                                       | Runtime startup, warmup, request construction, and ownership reset.                                |

In the reused intake burst, the current-thread runtime cannot consume commands during the synchronous producer loop.
The multi-thread runtime can consume them concurrently. Both cases measure submission intake, not completed work.
The concurrent-producer family keeps total work fixed at 64 and changes only the number of producer tasks.

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
