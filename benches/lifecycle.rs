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
    BackoffPolicy, EventKind, RestartPolicy, RuntimeError, Subscribe, Supervisor, TaskContext,
    TaskError, TaskFn, TaskOutcome, TaskRef, TaskSpec,
};

use support::fixtures::{
    AsyncCounter, EventCounter, RUNTIMES, bench_config, expect_canceled, expect_completed,
    expect_within, wait_for_ownership, warm_runtime,
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

const PERIODIC: CaseFamily = CaseFamily::lifecycle(
    "lifecycle/steady/finite_periodic_attempts",
    "FINITE PERIODIC / ALWAYS CYCLE",
    "observed attempt",
    "observed attempts",
    "watched admission through exactly 8 attempts and the terminal Canceled outcome; earlier successful attempts repeat under TaskSpec::periodic or RestartPolicy::Always",
    "runtime and Supervisor startup, warmup, TaskSpec construction, ownership reset, shutdown, and Tokio runtime construction",
)
.without_lifecycle_interpretation();

const REQUESTED_SHUTDOWN: CaseFamily = CaseFamily::lifecycle(
    "lifecycle/shutdown/requested_cooperative",
    "REQUESTED SHUTDOWN · COOPERATIVE TASKS",
    "completed shutdown",
    "completed shutdowns",
    "one SupervisorHandle::shutdown call from request through complete shared cleanup with 0 or 32 already-started cooperative tasks",
    "Tokio runtime and Supervisor startup, task construction and admission, started-task handshakes, waiter verification, and post-shutdown value disposal",
)
.without_lifecycle_interpretation();

const GRACE_EXCEEDED: CaseFamily = CaseFamily::lifecycle(
    "lifecycle/shutdown/grace_exceeded",
    "REQUESTED SHUTDOWN · GRACE EXCEEDED",
    "grace-expired shutdown",
    "grace-expired shutdowns",
    "one SupervisorHandle::shutdown call through GraceExceeded and force-abort commitment with 1 or 32 already-started non-cooperative tasks under one shared 10ms grace",
    "Tokio runtime and Supervisor startup, task construction and admission, started-task handshakes, waiter verification, and post-shutdown value disposal",
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

fn finite_repeat_task(attempts: Arc<AsyncCounter>, stop_after: usize) -> TaskRef {
    TaskFn::arc(move |_ctx: TaskContext| {
        let attempts = Arc::clone(&attempts);
        async move {
            if attempts.increment() == stop_after {
                Err(TaskError::Canceled)
            } else {
                Ok(())
            }
        }
    })
}

fn cooperative_shutdown_task(name: String, started: Arc<AsyncCounter>) -> TaskSpec {
    let task = TaskFn::arc(move |ctx: TaskContext| {
        let started = Arc::clone(&started);
        async move {
            started.increment();
            ctx.cancelled().await;
            Err(TaskError::Canceled)
        }
    });
    TaskSpec::once(name, task)
}

fn stubborn_shutdown_task(name: String, started: Arc<AsyncCounter>) -> TaskSpec {
    let task = TaskFn::arc(move |_ctx: TaskContext| {
        let started = Arc::clone(&started);
        async move {
            started.increment();
            std::future::pending::<()>().await;
            Ok(())
        }
    });
    TaskSpec::once(name, task)
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
                            let waiter = expect_within(
                                "watched admission",
                                handle.add(task).watch().execute(),
                            )
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
                        let waiter = expect_within(
                            "retry task admission",
                            handle.add(task).watch().execute(),
                        )
                        .await
                        .expect("retry task admission failed");
                        let id = waiter.id();
                        observer.wait_for_count(expected_events).await;
                        observer.assert_healthy();

                        let start = Instant::now();
                        let claimed =
                            expect_within("backoff cancellation", handle.cancel(id).execute())
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

fn bench_periodic(c: &mut Criterion) {
    const ATTEMPTS: usize = 8;
    let mut group = c.benchmark_group(PERIODIC.group_id);
    group.throughput(Throughput::Elements(ATTEMPTS as u64));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for policy in ["periodic_2ms", "always_no_interval"] {
            let parameter = format!("{ATTEMPTS}_attempts_{policy}");
            group.bench_function(BenchmarkId::new(rt_name, &parameter), |b| {
                record_case(PERIODIC, rt_name, Some(parameter.clone()));
                let rt = rt_fn();
                b.iter_custom(|iters| {
                    rt.block_on(async {
                        let handle = Supervisor::new(bench_config(), vec![])
                            .serve()
                            .expect("runtime startup");
                        warm_runtime(&handle, 0).await;
                        let mut total = Duration::ZERO;

                        for iteration in 0..iters {
                            let attempts = AsyncCounter::new();
                            let task = finite_repeat_task(Arc::clone(&attempts), ATTEMPTS);
                            let name = format!("finite-periodic-{policy}-{iteration}");
                            let spec = match policy {
                                "periodic_2ms" => {
                                    TaskSpec::periodic(name, task, Duration::from_millis(2))
                                }
                                "always_no_interval" => TaskSpec::restartable(name, task)
                                    .with_restart(RestartPolicy::Always { interval: None }),
                                _ => unreachable!("the benchmark declares both policies above"),
                            };

                            let start = Instant::now();
                            let waiter = expect_within(
                                "finite periodic task admission",
                                handle.add(spec).watch().execute(),
                            )
                            .await
                            .expect("finite periodic task admission failed");
                            let outcome = expect_within("finite periodic outcome", waiter.wait())
                                .await
                                .expect("finite periodic outcome channel closed");
                            total += start.elapsed();

                            assert!(
                                matches!(outcome, TaskOutcome::Canceled),
                                "finite periodic task must stop with Canceled, got {outcome:?}"
                            );
                            assert_eq!(attempts.load(), ATTEMPTS);
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

fn bench_requested_shutdown(c: &mut Criterion) {
    let mut group = c.benchmark_group(REQUESTED_SHUTDOWN.group_id);
    group.throughput(Throughput::Elements(1));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for task_count in [0usize, 32] {
            let parameter = format!("{task_count}_started_tasks");
            group.bench_function(BenchmarkId::new(rt_name, &parameter), |b| {
                record_case(REQUESTED_SHUTDOWN, rt_name, Some(parameter.clone()));
                let rt = rt_fn();
                b.iter_custom(|iters| {
                    rt.block_on(async {
                        let mut total = Duration::ZERO;
                        for iteration in 0..iters {
                            let supervisor = Supervisor::new(bench_config(), vec![]);
                            let handle = supervisor.serve().expect("runtime startup");
                            let started = AsyncCounter::new();
                            let mut waiters = Vec::with_capacity(task_count);
                            for i in 0..task_count {
                                let waiter = expect_within(
                                    "cooperative shutdown task admission",
                                    handle
                                        .add(cooperative_shutdown_task(
                                            format!("shutdown-cooperative-{iteration}-{i}"),
                                            Arc::clone(&started),
                                        ))
                                        .watch()
                                        .execute(),
                                )
                                .await
                                .expect("cooperative shutdown task admission failed");
                                waiters.push(waiter);
                            }
                            started.wait_for(task_count).await;

                            let start = Instant::now();
                            let result =
                                expect_within("cooperative requested shutdown", handle.shutdown())
                                    .await;
                            total += start.elapsed();
                            result.expect("cooperative requested shutdown failed");

                            for waiter in waiters {
                                expect_canceled(waiter).await;
                            }
                        }
                        total
                    })
                });
            });
        }
    }
    group.finish();
}

fn bench_grace_exceeded(c: &mut Criterion) {
    const GRACE: Duration = Duration::from_millis(10);
    let mut group = c.benchmark_group(GRACE_EXCEEDED.group_id);
    group.throughput(Throughput::Elements(1));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for task_count in [1usize, 32] {
            let parameter = format!("{task_count}_started_tasks_10ms_grace");
            group.bench_function(BenchmarkId::new(rt_name, &parameter), |b| {
                record_case(GRACE_EXCEEDED, rt_name, Some(parameter.clone()));
                let rt = rt_fn();
                b.iter_custom(|iters| {
                    rt.block_on(async {
                        let mut total = Duration::ZERO;
                        for iteration in 0..iters {
                            let config = bench_config().with_grace(GRACE);
                            let supervisor = Supervisor::new(config, vec![]);
                            let handle = supervisor.serve().expect("runtime startup");
                            let started = AsyncCounter::new();
                            let mut waiters = Vec::with_capacity(task_count);
                            for i in 0..task_count {
                                let waiter = expect_within(
                                    "stubborn shutdown task admission",
                                    handle
                                        .add(stubborn_shutdown_task(
                                            format!("shutdown-stubborn-{iteration}-{i}"),
                                            Arc::clone(&started),
                                        ))
                                        .watch()
                                        .execute(),
                                )
                                .await
                                .expect("stubborn shutdown task admission failed");
                                waiters.push(waiter);
                            }
                            started.wait_for(task_count).await;

                            let start = Instant::now();
                            let result = expect_within(
                                "grace-expired requested shutdown",
                                handle.shutdown(),
                            )
                            .await;
                            total += start.elapsed();
                            assert!(
                                matches!(result, Err(RuntimeError::GraceExceeded { .. })),
                                "stubborn tasks must exceed the shared grace, got {result:?}"
                            );

                            for waiter in waiters {
                                let outcome =
                                    expect_within("force-aborted shutdown outcome", waiter.wait())
                                        .await
                                        .expect("force-aborted outcome channel closed");
                                assert!(
                                    matches!(outcome, TaskOutcome::ForceAborted),
                                    "stubborn shutdown task must be force-aborted, got {outcome:?}"
                                );
                            }
                        }
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
    targets =
        bench_cold,
        bench_completion,
        bench_cancel_backoff,
        bench_periodic,
        bench_requested_shutdown,
        bench_grace_exceeded
}

fn main() {
    support::benchmark_main("lifecycle", benches);
}
