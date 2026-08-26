//! # Cold startup and steady lifecycle operations
//!
//! Keeps cold Supervisor startup separate from watched task completion, zero-delay retries,
//! and cancellation after a positive retry backoff is scheduled. No synthetic CPU loop is timed.
//!
//! Run with cargo bench --bench lifecycle.

mod support;

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use taskvisor::{
    BackoffPolicy, EventKind, Subscribe, Supervisor, TaskContext, TaskError, TaskFn, TaskSpec,
};

use support::fixtures::{
    EventCounter, RUNTIMES, bench_config, expect_canceled, expect_completed, expect_within,
    wait_for_ownership, warm_runtime,
};
use support::{CaseFamily, print_suite_header, record_case};

const COLD: CaseFamily = CaseFamily::lifecycle(
    "lifecycle/cold/verified_run",
    "COLD SUPERVISOR · ONE TASK",
    "completed task",
    "completed tasks",
    "fresh Supervisor construction through one successful task body and run's shared shutdown cleanup",
    "Tokio runtime and TaskSpec construction",
);

const COMPLETION: CaseFamily = CaseFamily::lifecycle(
    "lifecycle/steady/watched_completion",
    "STEADY SINGLE-TASK COMPLETION",
    "completed task",
    "completed tasks",
    "watched add through Completed; the retry variant fails twice before succeeding with zero-delay failure backoff",
    "runtime and Supervisor startup, warmup, TaskSpec construction, ownership reset, and shutdown",
);

const CANCEL_BACKOFF: CaseFamily = CaseFamily::lifecycle(
    "lifecycle/steady/cancel_scheduled_backoff",
    "CANCEL A SCHEDULED RETRY",
    "canceled task",
    "canceled tasks",
    "cancel after observing BackoffScheduled for a 60s retry delay through the Canceled outcome",
    "startup, warmup, first failure and backoff observation, task construction, ownership reset, and shutdown; the 60s delay is not awaited",
)
.without_lifecycle_interpretation();

fn retry_task(name: &str, failures: usize) -> (TaskSpec, Arc<AtomicUsize>) {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&attempts);
    let task = TaskFn::arc(move |_ctx: TaskContext| {
        let attempts = Arc::clone(&observed);
        async move {
            if attempts.fetch_add(1, Ordering::Relaxed) < failures {
                Err(TaskError::fail("benchmark retry"))
            } else {
                Ok(())
            }
        }
    });
    let spec = TaskSpec::restartable(name, task)
        .with_backoff(BackoffPolicy::constant(Duration::ZERO))
        .with_max_retries(NonZeroU32::new(2));
    (spec, attempts)
}

fn bench_cold(c: &mut Criterion) {
    print_suite_header("lifecycle");
    let mut group = c.benchmark_group(COLD.group_id);
    group.throughput(Throughput::Elements(1));

    for &(rt_name, rt_fn) in &RUNTIMES {
        group.bench_function(rt_name, |b| {
            record_case(COLD, rt_name, None);
            let rt = rt_fn();
            b.iter_custom(|iters| {
                rt.block_on(async {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let (task, attempts) = retry_task("cold", 0);
                        let start = Instant::now();
                        Supervisor::new(bench_config(), vec![])
                            .run(vec![task])
                            .await
                            .expect("cold lifecycle failed");
                        total += start.elapsed();
                        assert_eq!(attempts.load(Ordering::Relaxed), 1);
                    }
                    total
                })
            });
        });
    }
    group.finish();
}

fn bench_completion(c: &mut Criterion) {
    let mut group = c.benchmark_group(COMPLETION.group_id);
    group.throughput(Throughput::Elements(1));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for (label, failures) in [("first_attempt", 0), ("after_two_retries", 2)] {
            group.bench_function(BenchmarkId::new(rt_name, label), |b| {
                record_case(COMPLETION, rt_name, Some(label.to_owned()));
                let rt = rt_fn();
                b.iter_custom(|iters| {
                    rt.block_on(async {
                        let handle = Supervisor::new(bench_config(), vec![])
                            .serve()
                            .expect("runtime startup");
                        warm_runtime(&handle, 0).await;
                        let mut total = Duration::ZERO;

                        for _ in 0..iters {
                            let (task, attempts) = retry_task("watched", failures);
                            let start = Instant::now();
                            let (_, waiter) =
                                expect_within("watched admission", handle.add_and_watch(task))
                                    .await
                                    .expect("watched admission failed");
                            expect_completed(waiter).await;
                            total += start.elapsed();

                            assert_eq!(attempts.load(Ordering::Relaxed), failures + 1);
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

fn bench_cancel_backoff(c: &mut Criterion) {
    let mut group = c.benchmark_group(CANCEL_BACKOFF.group_id);
    group.throughput(Throughput::Elements(1));

    for &(rt_name, rt_fn) in &RUNTIMES {
        group.bench_function(rt_name, |b| {
            record_case(CANCEL_BACKOFF, rt_name, None);
            let rt = rt_fn();
            b.iter_custom(|iters| {
                rt.block_on(async {
                    let observer = EventCounter::new("backoff", EventKind::BackoffScheduled);
                    let subscribers: Vec<Arc<dyn Subscribe>> = vec![observer.clone()];
                    let handle = Supervisor::new(bench_config(), subscribers)
                        .serve()
                        .expect("runtime startup");
                    warm_runtime(&handle, 1).await;
                    let mut total = Duration::ZERO;

                    for _ in 0..iters {
                        let (task, attempts) = retry_task("backoff", 2);
                        let task =
                            task.with_backoff(BackoffPolicy::constant(Duration::from_secs(60)));
                        let expected_events = observer.count() + 1;
                        let (id, waiter) =
                            expect_within("retry task admission", handle.add_and_watch(task))
                                .await
                                .expect("retry task admission failed");
                        observer.wait_for_count(expected_events).await;
                        observer.assert_healthy();

                        let start = Instant::now();
                        let claimed = expect_within("backoff cancellation", handle.cancel(id))
                            .await
                            .expect("backoff cancellation failed");
                        expect_canceled(waiter).await;
                        total += start.elapsed();

                        assert!(claimed, "cancel did not claim the retrying task");
                        assert_eq!(attempts.load(Ordering::Relaxed), 1);
                        assert_eq!(observer.count(), expected_events);
                        wait_for_ownership(&handle, 1).await;
                    }

                    observer.assert_healthy();
                    handle.shutdown().await.expect("shutdown failed");
                    total
                })
            });
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = support::fixtures::criterion();
    targets = bench_cold, bench_completion, bench_cancel_backoff
}

fn main() {
    support::benchmark_main("lifecycle", benches);
}
