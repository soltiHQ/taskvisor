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

Results with the same measured boundary share one card. Runtime and batch-size results appear as separate entries inside that card.

- `Results` is the number of individual runtime and batch-size measurements.
- `Groups` is the number of shared cards in the report.

- `completed tasks/s` is the number of complete task lifecycles per second.
- `amortized per completed task` is the batch time divided by the number of tasks.
- `for the complete batch` is the measured wall time for the whole batch.
- `95% CI` is the confidence interval reported by Criterion.
- `Boundary` says where timing starts and ends.
- `Outside` lists work that was not timed.

Read `Boundary` and `Outside` before comparing two cases. An intake result measures accepted calls, not completed tasks. 
A policy result measures verified controller decisions. A query result measures snapshot calls.

For a batch, the report shows both total time and average time per item. 
The average is not the latency of one task inside a concurrent batch.

Criterion prints the generic unit `elem/s`. The Taskvisor snapshot replaces it with the real unit,
such as completed tasks, accepted submissions, rejections, or snapshot calls.

`Performance has regressed`, `improved`, and `no change` compare the result with a saved Criterion
baseline. They do not change the absolute time or operation rate from the current run.

In `controller/steady/intake_try_submit`, the current-thread case measures one producer burst. 
The controller can consume submissions at the same time only in the multi-thread case.

## Project guide

For cases that measure one complete managed-task lifecycle, Taskvisor uses this reading guide:

| Completed task lifecycles | Project reading                 |
|---------------------------|---------------------------------|
| Below 10 K/s              | Below the high-throughput range |
| 10 K/s to below 50 K/s    | Substantial throughput          |
| 50 K/s to below 200 K/s   | High-throughput range           |
| 200 K/s and above         | Very-high-throughput range      |

When every result in a card falls in the same range, the card prints one shared project reading.
`VARIES BY RESULT` means that results in the card fall in different ranges. 
`BAND EDGE` means that the 95% confidence interval crosses 10 K/s, 50 K/s, or 200 K/s. 
It is not a benchmark failure.

This guide applies only to complete managed-task lifecycles. 
It does not apply to intake, policy, or query results. 
It is a Taskvisor project guide, not an industry standard, SLO, certification, or production capacity promise.

## What the result means

A result describes the exact benchmark case on the machine that ran it. 
It is useful for comparing Taskvisor operations and for finding the cost of a specific lifecycle boundary.

It does not prove how much traffic an application can handle. 
Real task work, contention, subscribers, cancellation, memory use, and safety headroom belong in an application load test.
