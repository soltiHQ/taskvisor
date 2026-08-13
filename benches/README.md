# Reading Taskvisor benchmarks

Every suite prints a host and build header before Criterion starts. The case name states whether it
uses a fresh unprewarmed supervisor, steady-state intake, a verified policy decision, or a complete
task lifecycle.

After the statistical run, Taskvisor prints a color-aware performance snapshot with semantic units,
amortized cost, whole-batch latency, the confidence interval, the exact timing boundary, and a bottom
line. Completed managed-task cases receive the optional project interpretation. Lifecycle cases
with other units stay neutral. Partial paths are never presented as completed-task throughput.
Color follows terminal support, `NO_COLOR`, and Criterion's
`--color auto|always|never` option; every badge remains readable without color.

## Criterion fields

- `time` is wall time for one named operation or one whole named batch.
- `thrpt` is the number of semantic units completed per second.
- Criterion renders every semantic unit as `elem/s`. The final Taskvisor snapshot defines whether
  one element is a completed task, accepted submission, verified rejection, snapshot call, or
  add/cancel cycle.
- With the suite's default Criterion confidence level, the three values in brackets are the 95%
  confidence lower bound, point estimate, and 95% confidence upper bound.
- `Performance has regressed`, `improved`, and `no change` compare with a stored local baseline.
  They do not change the absolute `time` and `thrpt` values.

For a batch case, divide `time` by the count in the case name to get amortized time per unit. This is
not the latency observed by one task inside a concurrent or pipelined batch. Criterion already
performs the inverse calculation for `thrpt`.

| Rate | Amortized time per unit |
|------|-----------------------|
| 1 K/s | 1 ms |
| 10 K/s | 100 µs |
| 100 K/s | 10 µs |
| 1 M/s | 1 µs |

Example: a 50-task case at `1 ms` and `50 Kelem/s` completed the full 50-task batch in `1 ms`.
Its amortized cost was `20 µs` per completed task.

## Measurement boundaries

| Case prefix | Timed boundary | Criterion element | Important exclusions |
|-------------|----------------|-------------------|----------------------|
| `lifecycle/cold/full_run` | Fresh `Supervisor` construction through one final outcome and shared cleanup | completed task | task value and Tokio runtime construction |
| `throughput/cold/full_batch` | Fresh `Supervisor` construction through batch completion and shared cleanup; the CPU case runs 1,000 loop iterations per task | completed task | task values, subscriber value, and Tokio runtime construction |
| `fanout/cold/full_batch` | Fresh `Supervisor` construction through 100 task completions, minimal counting callbacks, drain, and cleanup | completed task | task values, subscriber values, and Tokio runtime construction |
| `dynamic/steady/sustained_registry_add` | Repeated prewarmed `add` calls through ownership admission and registry acceptance | accepted add call | supervisor startup, warmup, task construction, final drain, shutdown, and Tokio runtime construction; prior task lifecycles can apply backpressure |
| `dynamic/steady/add_cancel` | One prewarmed add through terminal cancel confirmation | add/cancel cycle | supervisor startup, warmup, task construction, shutdown, and Tokio runtime construction |
| `dynamic/steady/list_snapshot` | One registry snapshot with the named registered-task count | snapshot call | supervisor startup, registry prepopulation, shutdown, and Tokio runtime construction |
| `dynamic/cold/add_shutdown` | First add through cleanup of the named task batch during shutdown | cleaned task | `Supervisor` construction/startup, task construction, and Tokio runtime construction |
| `controller/cold/first_try_submit` | First caller-side `try_submit` on a fresh served supervisor | accepted intake | runtime and supervisor/controller startup, request construction, controller decision, task result, and shutdown |
| `controller/steady/intake_try_submit` | Prewarmed caller-side `try_submit` burst | accepted intake | runtime and supervisor/controller startup, warmup, request construction, controller decisions, task results, and shutdown |
| `controller/steady/drop_busy_rejection` | Watched intake through verified `SlotBusy` outcomes | verified rejection | runtime and supervisor/controller startup, owner setup/release/cleanup, and request construction |
| `controller/steady/replace_busy_placement` | `N` watched Replace submissions through `N - 1` verified displaced outcomes and retention of the newest request | processed replacement | runtime and supervisor/controller startup, owner setup/release, request construction, and newest task completion |
| `controller/steady/queue_one_slot` | Watched intake through final outcomes for one slot | completed task | runtime and supervisor/controller startup, warmup, request construction, and shutdown |
| `controller/steady/queue_eight_slots` | Watched intake through final outcomes across eight slots | completed task | runtime and supervisor/controller startup, warmup, request construction, and shutdown |

`current_thread` and `multi_thread` describe the Tokio runtime. The multi-thread runtime has four
Tokio workers. Taskvisor can also own cleanup workers and subscriber callback threads in either case.

`cold` means that the supervisor is fresh and unprewarmed. Its exact stopwatch boundary is
case-specific and is listed in the table.

## Optional project heuristic

For full-lifecycle cases whose semantic unit is one completed managed task, this guide offers a
descriptive scale:

| Completed task lifecycles | Project interpretation |
|---------------------------|------------------------|
| below 10 K/s | below the high-throughput range |
| 10 K/s to below 50 K/s | substantial lifecycle throughput |
| 50 K/s to below 200 K/s | high-throughput territory |
| 200 K/s and above | very-high-throughput territory |

This is a Taskvisor project interpretation, not an industry standard, SLO, capacity promise, or
production-readiness verdict. Do not apply it to intake, policy rejection, snapshot, or other partial
boundaries. Application load tests still decide usable production capacity.

## What the numbers can establish

The results establish absolute latency and operation rate for the exact synthetic boundary on the
machine that ran the suite. They let a reader distinguish, for example, one million accepted intake
calls per second from one hundred thousand completed task lifecycles per second.

They do not establish an application-wide capacity by themselves. Real task work, contention,
subscriber behavior, cancellation, memory pressure, and required headroom belong to the application
load test. Record the CPU model, operating system, Taskvisor revision, enabled features, and benchmark
command when publishing results. The console header prints architecture, operating system, logical
CPU count, and crate version; it cannot identify every build or host detail.
If automatic CPU detection is unavailable, set `TASKVISOR_BENCH_CPU` before running the suite.

For `controller/steady/intake_try_submit`, the `current_thread` case measures a producer burst while
the controller cannot consume on that same thread. The `multi_thread` case can consume concurrently.

## Commands

Run every suite:

```bash
cargo bench --all-features
```

Print the cleanest product-facing snapshot while retaining statistical estimates:

```bash
cargo bench --bench controller --features controller -- \
  'controller/steady/queue_one_slot/current_thread/20_completed_tasks' \
  --exact --quiet --color always
```

Run the comprehensive five-suite report:

```bash
cargo bench --all-features -- --quiet --color always
```

Run one suite:

```bash
cargo bench --bench controller --features controller
```

Filter by a full or partial Criterion case name:

```bash
cargo bench --bench controller --features controller -- 'steady/queue_one_slot'
```
