# Taskvisor benchmarks

These benchmarks show how fast Taskvisor completes specific operations on the machine that runs them.
Each result states what was timed and what stayed outside the timer.

## Run the benchmarks

Run all five benchmark suites with the colored Taskvisor report:

```bash
task rust:benchmark
```

The equivalent Cargo command is:

```bash
cargo bench --bench '*' --features controller --locked -- --quiet --color always
```

Run one suite:

```bash
cargo bench --bench controller --features controller -- --color always
```

Run matching cases from one suite:

```bash
cargo bench --bench controller --features controller -- 'steady/queue_one_slot'
```

## What each suite measures

| Suite        | What it measures                                                              |
|--------------|-------------------------------------------------------------------------------|
| `lifecycle`  | One task from a fresh supervisor to its final result and cleanup.             |
| `throughput` | Complete batches of instant tasks or tasks with small CPU work.               |
| `fanout`     | Complete task batches with 0, 1, 4, or 8 subscribers.                         |
| `dynamic`    | Adding, canceling, listing, and cleaning up tasks through `SupervisorHandle`. |
| `controller` | Submission intake, queueing, replacement, rejection, and completed tasks.     |

Case names use a few common labels:

- `cold`: the case uses a fresh supervisor.
- `steady`: Taskvisor is warmed up before timing starts.
- `current_thread`: Tokio uses its current-thread runtime.
- `multi_thread`: Tokio uses four worker threads.

The runtime label describes Tokio only. Taskvisor may also use cleanup workers and subscriber threads.

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

Criterion prints the generic unit `elem/s`. The Taskvisor snapshot replaces it with the real unit,
such as completed tasks, accepted submissions, rejections, or snapshot calls.

`Performance has regressed`, `improved`, and `no change` compare the result with a saved Criterion
baseline. They do not change the absolute time or operation rate from the current run.

In `controller/steady/intake_try_submit`, the current-thread case measures one producer burst.
The controller can consume submissions at the same time only in the multi-thread case.

## Scope labels

The report classifies results by what their rate counts:

| Scope label                                     | Meaning                                                          |
|-------------------------------------------------|------------------------------------------------------------------|
| `COMPLETE MANAGED-TASK LIFECYCLE`               | Each unit is one Taskvisor-managed task completed end to end.    |
| `COMPLETE LIFECYCLE · <NAMED UNIT>`             | The label states the completed unit, such as management cycles.  |
| `OPERATION RATE, NOT COMPLETED-TASK THROUGHPUT` | The case measures intake, policy decisions, or query calls.      |

These labels describe the measured unit. They do not rate the result as high or low.
Card colors distinguish measurement scopes. They do not grade performance.

## What the result means

A result describes the exact benchmark case on the machine that ran it.
It is useful for comparing repeated runs of the same case and examining one named boundary.

It does not prove how much traffic an application can handle.
Real task work, contention, subscribers, cancellation, memory use, and safety headroom belong in an application load test.
