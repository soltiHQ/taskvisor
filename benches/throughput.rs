//! # Steady task completion throughput
//!
//! Measures watched batches on a prewarmed supervisor without subscribers.
//! Every task must complete; deferred ownership cleanup finishes between batches outside the timer.
//! The steady families include admission. The drain families admit every task before the timer,
//! observe their case-specific initial readiness, then release cooperative CPU work or a saturated
//! concurrency limit.
//!
//! Run with cargo bench --bench throughput.

mod support;

use std::future::{Future, poll_fn};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::Poll;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use taskvisor::{Supervisor, TaskFn, TaskOutcome, TaskRef, TaskSpec, TaskWaiter};

use support::fixtures::{
    AsyncCounter, AsyncFlag, RUNTIMES, WATCHDOG, bench_config, complete_batch, expect_within,
    instant_task, wait_for_ownership, warm_runtime,
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

const MAX_CONCURRENT_ENABLED_OVERHEAD: CaseFamily = CaseFamily::lifecycle(
    "throughput/steady/max_concurrent_batch",
    "MAX_CONCURRENT · ENABLED-PATH OVERHEAD",
    "completed task",
    "completed tasks",
    "first watched add through 256 Completed outcomes for instant tasks with max_concurrent disabled or set to 1, 4, or 256; no pre-timer saturation state is established",
    "runtime and Supervisor startup, warmup, TaskSpec construction, ownership reset between batches, shutdown, Tokio runtime construction, and any assertion about concurrent task-body entry",
);

const COOPERATIVE_DRAIN: CaseFamily = CaseFamily::drain(
    "throughput/drain/cooperative_cpu_batch",
    "PRE-ADMITTED COOPERATIVE CPU WORK · DRAIN",
    "drained task",
    "drained tasks",
    "one shared release through all Completed outcomes for 64 already-admitted tasks whose bodies have reached the release gate; each task performs 16 CPU chunks separated by cooperative yields",
    "runtime and Supervisor startup, warmup, TaskSpec construction, admission, task-body entry handshake, outcome-vector allocation, watchdog registration, result validation, ownership reset, and shutdown",
);

const MAX_CONCURRENT_SATURATED: CaseFamily = CaseFamily::drain(
    "throughput/drain/max_concurrent_saturated",
    "MAX_CONCURRENT · SATURATED DRAIN",
    "drained task",
    "drained tasks",
    "one shared release through 64 Completed outcomes after task-body handshakes observe exactly min(limit, 64) entered tasks and, when limit is lower than 64, the remaining admitted tasks have not entered",
    "runtime and Supervisor startup, warmup, TaskSpec construction, admission, pre-release task-body entry and stability handshake, outcome-vector allocation, watchdog registration, outcome validation, ownership reset, shutdown, and Tokio runtime construction",
);

const DRAIN_COUNT: usize = 64;
const CPU_CHUNKS: usize = 16;
const CPU_STEPS_PER_CHUNK: usize = 4_096;

async fn admit_watched_batch(
    handle: &taskvisor::SupervisorHandle,
    tasks: Vec<TaskSpec>,
) -> Vec<TaskWaiter> {
    expect_within("pre-timed watched batch admission", async {
        let mut waiters = Vec::with_capacity(tasks.len());
        for task in tasks {
            let (_, waiter) = handle
                .add_and_watch(task)
                .await
                .expect("batch admission failed");
            waiters.push(waiter);
        }
        waiters
    })
    .await
}

async fn release_and_receive(
    release: &AsyncFlag,
    waiters: Vec<TaskWaiter>,
) -> (Vec<TaskOutcome>, Duration) {
    let mut outcomes = Vec::with_capacity(waiters.len());
    let mut watchdog = Box::pin(tokio::time::sleep(WATCHDOG));
    poll_fn(|cx| {
        assert!(
            watchdog.as_mut().poll(cx).is_pending(),
            "benchmark watchdog expired before the timed drain"
        );
        Poll::Ready(())
    })
    .await;
    let drain = async move {
        for waiter in waiters {
            outcomes.push(waiter.wait().await.expect("batch outcome channel closed"));
        }
        outcomes
    };
    tokio::pin!(drain);

    let start = Instant::now();
    release.mark();
    let outcomes = tokio::select! {
        biased;
        outcomes = &mut drain => outcomes,
        _ = &mut watchdog => panic!("benchmark timed out while draining a pre-admitted task batch"),
    };
    let elapsed = start.elapsed();
    (outcomes, elapsed)
}

fn assert_completed(outcomes: Vec<TaskOutcome>) {
    for outcome in outcomes {
        assert!(
            matches!(outcome, TaskOutcome::Completed),
            "expected Completed, got {outcome:?}"
        );
    }
}

async fn observe_stable_entry_count(entered: &AsyncCounter, expected: usize, total: usize) {
    entered.wait_for(expected).await;
    for _ in 0..total {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        entered.load(),
        expected,
        "task-body entry count changed before the timed release"
    );
}

async fn observe_saturated_entry_count(
    handle: &taskvisor::SupervisorHandle,
    entered: &AsyncCounter,
    expected: usize,
    total: usize,
) {
    observe_stable_entry_count(entered, expected, total).await;
    let alive = handle.alive_snapshot().await;
    assert_eq!(
        alive.len(),
        expected,
        "alive_snapshot must match the observed task-body entry count before release"
    );
    assert_eq!(
        entered.load(),
        expected,
        "task-body entry count changed while observing active attempts"
    );
}

fn cooperative_cpu_task(
    name: String,
    index: usize,
    entered: Arc<AsyncCounter>,
    release: Arc<AsyncFlag>,
    results: Arc<Vec<AtomicU64>>,
) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(move |_ctx| {
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        let results = Arc::clone(&results);
        async move {
            entered.increment();
            release.wait().await;

            let mut value =
                std::hint::black_box((index as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15));
            for chunk in 0..CPU_CHUNKS {
                for step in 0..CPU_STEPS_PER_CHUNK {
                    value = value
                        .wrapping_mul(0x5851_f42d_4c95_7f2d)
                        .wrapping_add((step as u64) ^ chunk as u64)
                        .rotate_left(13);
                }
                value = std::hint::black_box(value);
                tokio::task::yield_now().await;
            }
            results[index].store(value | 1, Ordering::Release);
            Ok(())
        }
    });
    TaskSpec::once(name, task)
}

fn gated_yielding_task(
    name: String,
    entered: Arc<AsyncCounter>,
    release: Arc<AsyncFlag>,
) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(move |_ctx| {
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        async move {
            entered.increment();
            release.wait().await;
            tokio::task::yield_now().await;
            Ok(())
        }
    });
    TaskSpec::once(name, task)
}

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
    let mut group = c.benchmark_group(MAX_CONCURRENT_ENABLED_OVERHEAD.group_id);
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
                record_case(
                    MAX_CONCURRENT_ENABLED_OVERHEAD,
                    rt_name,
                    Some(parameter.clone()),
                );
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

fn bench_cooperative_drain(c: &mut Criterion) {
    let mut group = c.benchmark_group(COOPERATIVE_DRAIN.group_id);
    group.throughput(Throughput::Elements(DRAIN_COUNT as u64));

    for &(rt_name, rt_fn) in &RUNTIMES {
        let parameter =
            format!("{DRAIN_COUNT}_tasks_{CPU_CHUNKS}_chunks_{CPU_STEPS_PER_CHUNK}_steps");
        group.bench_function(BenchmarkId::new(rt_name, &parameter), |b| {
            record_case(COOPERATIVE_DRAIN, rt_name, Some(parameter.clone()));
            let rt = rt_fn();
            b.iter_custom(|iters| {
                rt.block_on(async {
                    let handle = Supervisor::new(bench_config(), vec![])
                        .serve()
                        .expect("runtime startup");
                    warm_runtime(&handle, 0).await;
                    let mut total = Duration::ZERO;

                    for iteration in 0..iters {
                        let entered = AsyncCounter::new();
                        let release = AsyncFlag::new();
                        let results = Arc::new(
                            (0..DRAIN_COUNT)
                                .map(|_| AtomicU64::new(0))
                                .collect::<Vec<_>>(),
                        );
                        let tasks = (0..DRAIN_COUNT)
                            .map(|i| {
                                cooperative_cpu_task(
                                    format!("cooperative-drain-{iteration}-{i}"),
                                    i,
                                    Arc::clone(&entered),
                                    Arc::clone(&release),
                                    Arc::clone(&results),
                                )
                            })
                            .collect();
                        let waiters = admit_watched_batch(&handle, tasks).await;
                        observe_stable_entry_count(&entered, DRAIN_COUNT, DRAIN_COUNT).await;

                        let (outcomes, elapsed) = release_and_receive(&release, waiters).await;
                        total += elapsed;

                        assert_completed(outcomes);
                        assert!(
                            results
                                .iter()
                                .all(|result| result.load(Ordering::Acquire) != 0),
                            "every cooperative task must publish its CPU result"
                        );
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

fn bench_saturated_max_concurrent(c: &mut Criterion) {
    let mut group = c.benchmark_group(MAX_CONCURRENT_SATURATED.group_id);
    group.throughput(Throughput::Elements(DRAIN_COUNT as u64));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for limit in [1usize, 4, DRAIN_COUNT] {
            let parameter = format!("{DRAIN_COUNT}_admitted_tasks_limit_{limit}");
            group.bench_function(BenchmarkId::new(rt_name, &parameter), |b| {
                record_case(MAX_CONCURRENT_SATURATED, rt_name, Some(parameter.clone()));
                let rt = rt_fn();
                b.iter_custom(|iters| {
                    rt.block_on(async {
                        let config = bench_config().with_max_concurrent(NonZeroUsize::new(limit));
                        let handle = Supervisor::new(config, vec![])
                            .serve()
                            .expect("runtime startup");
                        warm_runtime(&handle, 0).await;
                        let mut total = Duration::ZERO;

                        for iteration in 0..iters {
                            let entered = AsyncCounter::new();
                            let release = AsyncFlag::new();
                            let tasks = (0..DRAIN_COUNT)
                                .map(|i| {
                                    gated_yielding_task(
                                        format!("saturated-limit-{limit}-{iteration}-{i}"),
                                        Arc::clone(&entered),
                                        Arc::clone(&release),
                                    )
                                })
                                .collect();
                            let waiters = admit_watched_batch(&handle, tasks).await;
                            let expected_entered = limit.min(DRAIN_COUNT);
                            observe_saturated_entry_count(
                                &handle,
                                &entered,
                                expected_entered,
                                DRAIN_COUNT,
                            )
                            .await;

                            let (outcomes, elapsed) = release_and_receive(&release, waiters).await;
                            total += elapsed;

                            assert_completed(outcomes);
                            assert_eq!(
                                entered.load(),
                                DRAIN_COUNT,
                                "every admitted task must enter its body during the drain"
                            );
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
    targets = bench_completion, bench_max_concurrent, bench_cooperative_drain, bench_saturated_max_concurrent
}

fn main() {
    support::benchmark_main("throughput", benches);
}
