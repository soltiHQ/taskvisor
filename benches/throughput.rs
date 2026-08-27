//! # Steady task completion throughput
//!
//! Measures watched batches on a prewarmed supervisor without subscribers or application work.
//! Every task must complete; deferred ownership cleanup finishes between batches outside the timer.
//! Matched yielding tasks measure successful execution with and without a polled deadline.
//! Neither variant measures deadline expiry or application I/O latency.
//!
//! Run with cargo bench --bench throughput.

mod support;

use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use taskvisor::{Supervisor, TaskFn, TaskSpec};

use support::fixtures::{
    RUNTIMES, bench_config, complete_batch, instant_task, wait_for_ownership, warm_runtime,
};
use support::{CaseFamily, print_suite_header, record_case};

const COMPLETED: CaseFamily = CaseFamily::lifecycle(
    "throughput/steady/watched_batch",
    "STEADY TASK COMPLETION",
    "completed task",
    "completed tasks",
    "first watched add through all Completed outcomes in a fixed batch without subscribers",
    "runtime and Supervisor startup, warmup, TaskSpec construction, ownership reset between batches, and shutdown",
);

const YIELDING: CaseFamily = CaseFamily::lifecycle(
    "throughput/steady/yielding_batch",
    "STEADY TASK COMPLETION · ONE YIELD",
    "completed task",
    "completed tasks",
    "first watched add through 256 Completed outcomes; each task explicitly yields once before succeeding",
    "runtime and Supervisor startup, warmup, TaskSpec construction, ownership reset between batches, and shutdown",
);

const WITH_DEADLINE: CaseFamily = CaseFamily::lifecycle(
    "throughput/steady/yielding_batch_with_deadline",
    "ONE YIELD · DEADLINE ENABLED",
    "completed task",
    "completed tasks",
    "first watched add through 256 Completed outcomes; each task yields once with a 60s attempt deadline, ensuring the timer is polled but never expires",
    "runtime and Supervisor startup, warmup, TaskSpec construction, ownership reset between batches, and shutdown",
);

const MAX_CONCURRENT: CaseFamily = CaseFamily::lifecycle(
    "throughput/steady/max_concurrent_batch",
    "STEADY TASK COMPLETION · MAX_CONCURRENT",
    "completed task",
    "completed tasks",
    "first watched add through 256 Completed outcomes with max_concurrent disabled or set to 1, 4, or 256",
    "runtime and Supervisor startup, warmup, TaskSpec construction, ownership reset between batches, shutdown, and Tokio runtime construction",
);

fn bench_completion(c: &mut Criterion) {
    const COUNT: usize = 256;
    print_suite_header("throughput");

    for (family, yields, deadline) in [
        (COMPLETED, false, None),
        (YIELDING, true, None),
        (WITH_DEADLINE, true, Some(Duration::from_secs(60))),
    ] {
        let mut group = c.benchmark_group(family.group_id);
        group.throughput(Throughput::Elements(COUNT as u64));
        for &(rt_name, rt_fn) in &RUNTIMES {
            let parameter = format!("{COUNT}_completed_tasks");
            group.bench_function(BenchmarkId::new(rt_name, &parameter), |b| {
                record_case(family, rt_name, Some(parameter.clone()));
                let rt = rt_fn();
                b.iter_custom(|iters| {
                    rt.block_on(async {
                        let handle = Supervisor::new(bench_config(), vec![])
                            .serve()
                            .expect("runtime startup");
                        warm_runtime(&handle, 0).await;
                        let mut total = Duration::ZERO;

                        for _ in 0..iters {
                            let tasks = (0..COUNT)
                                .map(|i| {
                                    let name = format!("batch-{i}");
                                    let task = if yields {
                                        TaskSpec::once(
                                            name,
                                            TaskFn::arc(|_| async {
                                                tokio::task::yield_now().await;
                                                Ok(())
                                            }),
                                        )
                                    } else {
                                        instant_task(name)
                                    };
                                    task.with_timeout(deadline)
                                })
                                .collect();

                            let start = Instant::now();
                            complete_batch(&handle, tasks).await;
                            total += start.elapsed();

                            wait_for_ownership(&handle, 0).await;
                        }

                        handle.shutdown().await.expect("shutdown failed");
                        total
                    })
                });
            });
        }
        group.finish();
    }
}

fn bench_max_concurrent(c: &mut Criterion) {
    const COUNT: usize = 256;
    let mut group = c.benchmark_group(MAX_CONCURRENT.group_id);
    group.throughput(Throughput::Elements(COUNT as u64));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for (limit_name, limit) in [
            ("unlimited", None),
            ("limit_1", Some(1usize)),
            ("limit_4", Some(4usize)),
            ("limit_256", Some(COUNT)),
        ] {
            let parameter = format!("{COUNT}_completed_tasks_{limit_name}");
            group.bench_function(BenchmarkId::new(rt_name, &parameter), |b| {
                record_case(MAX_CONCURRENT, rt_name, Some(parameter.clone()));
                let rt = rt_fn();
                b.iter_custom(|iters| {
                    rt.block_on(async {
                        let configured_limit = limit.and_then(NonZeroUsize::new);
                        let config = bench_config().with_max_concurrent(configured_limit);
                        let handle = Supervisor::new(config, vec![])
                            .serve()
                            .expect("runtime startup");
                        warm_runtime(&handle, 0).await;
                        let mut total = Duration::ZERO;

                        for iteration in 0..iters {
                            let tasks = (0..COUNT)
                                .map(|i| {
                                    instant_task(format!(
                                        "max-concurrent-{limit_name}-{iteration}-{i}"
                                    ))
                                })
                                .collect();

                            let start = Instant::now();
                            complete_batch(&handle, tasks).await;
                            total += start.elapsed();

                            wait_for_ownership(&handle, 0).await;
                        }

                        handle.shutdown().await.expect("shutdown failed");
                        total
                    })
                });
            });
        }
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = support::fixtures::criterion();
    targets = bench_completion, bench_max_concurrent
}

fn main() {
    support::benchmark_main("throughput", benches);
}
