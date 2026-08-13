//! # Controller admission and lifecycle benchmarks
//!
//! Separates first-use startup, caller-side intake, verified policy decisions, and complete
//! controller-managed task lifecycles. Every measured operation is checked; rejected, timed-out,
//! or incomplete work cannot silently enter the statistics.
//!
//! Run with `cargo bench --bench controller --features controller`.

mod support;

use std::hint::black_box;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use taskvisor::{
    BackoffPolicy, ControllerConfig, ControllerSpec, RejectionKind, RestartPolicy, Supervisor,
    SupervisorConfig, SupervisorHandle, TaskContext, TaskFn, TaskOutcome, TaskRef, TaskSpec,
    TaskWaiter,
};
use tokio::runtime::Runtime;
use tokio::sync::Notify;

use support::{CaseFamily, print_suite_header, record_case};

const COLD_INTAKE: CaseFamily = CaseFamily::intake(
    "controller/cold/first_try_submit",
    "COLD FIRST TRY_SUBMIT",
    "accepted submission",
    "accepted submissions",
    "first caller-side try_submit on a fresh served supervisor, including lazy cleanup-worker startup",
    "Supervisor/controller startup, request construction, controller decision, task outcome, shutdown, and Tokio runtime construction",
);

const STEADY_INTAKE: CaseFamily = CaseFamily::intake(
    "controller/steady/intake_try_submit",
    "STEADY TRY_SUBMIT BURST",
    "accepted submission",
    "accepted submissions",
    "prewarmed caller-side try_submit burst",
    "Supervisor/controller startup, warmup, request construction, controller decisions, task outcomes, shutdown, and Tokio runtime construction",
);

const DROP_REJECTION: CaseFamily = CaseFamily::policy(
    "controller/steady/drop_busy_rejection",
    "DROP_IF_RUNNING · BUSY SLOT",
    "verified rejection",
    "verified rejections",
    "watched intake through verified SlotBusy outcomes",
    "Supervisor/controller startup, held-owner setup/release/cleanup, request construction, and Tokio runtime construction",
);

const REPLACE_PLACEMENT: CaseFamily = CaseFamily::policy(
    "controller/steady/replace_busy_placement",
    "REPLACE · BUSY SLOT",
    "processed replacement",
    "processed replacements",
    "N watched Replace submissions through N-1 SupersededByReplace outcomes and retention of the newest request",
    "Supervisor/controller startup, held-owner setup/release, request construction, newest task completion, and Tokio runtime construction",
);

const QUEUE_ONE: CaseFamily = CaseFamily::lifecycle(
    "controller/steady/queue_one_slot",
    "QUEUE · ONE SLOT",
    "completed task",
    "completed tasks",
    "watched controller intake through final outcomes in one slot",
    "Supervisor/controller startup, warmup, request construction, shutdown, and Tokio runtime construction",
);

const QUEUE_EIGHT: CaseFamily = CaseFamily::lifecycle(
    "controller/steady/queue_eight_slots",
    "QUEUE · EIGHT SLOTS",
    "completed task",
    "completed tasks",
    "watched controller intake through final outcomes across eight slots",
    "Supervisor/controller startup, warmup, request construction, shutdown, and Tokio runtime construction",
);

fn rt_current_thread() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn rt_multi_thread() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
}

type RtFactory = fn() -> Runtime;

const RUNTIMES: [(&str, RtFactory); 2] = [
    ("current_thread", rt_current_thread as RtFactory),
    ("multi_thread", rt_multi_thread as RtFactory),
];

fn bench_config() -> SupervisorConfig {
    SupervisorConfig::default()
        .with_bus_capacity(NonZeroUsize::new(16384).unwrap())
        .with_grace(Duration::from_secs(5))
}

fn instant_task(name: impl Into<Arc<str>>) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(|_ctx: TaskContext| async { Ok(()) });
    TaskSpec::new(
        name,
        task,
        RestartPolicy::Never,
        BackoffPolicy::default(),
        None,
    )
}

struct AsyncFlag {
    set: AtomicBool,
    changed: Notify,
}

impl AsyncFlag {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            set: AtomicBool::new(false),
            changed: Notify::new(),
        })
    }

    fn mark(&self) {
        self.set.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }

    async fn wait(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.set.load(Ordering::Acquire) {
                return;
            }
            changed.await;
        }
    }
}

fn held_owner_task(
    name: impl Into<Arc<str>>,
    started: Arc<AsyncFlag>,
    release: Arc<AsyncFlag>,
) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(move |ctx: TaskContext| {
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        async move {
            started.mark();
            ctx.cancelled().await;
            release.wait().await;
            Ok(())
        }
    });
    TaskSpec::new(
        name,
        task,
        RestartPolicy::Never,
        BackoffPolicy::default(),
        None,
    )
}

async fn expect_within<F, T>(label: &str, future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .unwrap_or_else(|_| panic!("benchmark timed out while waiting for {label}"))
}

async fn expect_completed(waiter: TaskWaiter) {
    let outcome = expect_within("a completed task outcome", waiter.wait())
        .await
        .expect("task outcome channel closed");
    assert!(
        matches!(outcome, TaskOutcome::Completed),
        "expected Completed, got {outcome:?}"
    );
}

async fn expect_canceled(waiter: TaskWaiter) {
    let outcome = expect_within("a canceled owner outcome", waiter.wait())
        .await
        .expect("owner outcome channel closed");
    assert!(
        matches!(outcome, TaskOutcome::Canceled),
        "expected Canceled, got {outcome:?}"
    );
}

async fn expect_rejected(waiter: TaskWaiter, expected: RejectionKind) {
    let outcome = expect_within("a controller rejection", waiter.wait())
        .await
        .expect("submission outcome channel closed");
    assert!(
        matches!(outcome, TaskOutcome::Rejected { kind, .. } if kind == expected),
        "expected {expected:?}, got {outcome:?}"
    );
}

async fn warm_controller(handle: &SupervisorHandle, label: &str) {
    let (_, waiter) = handle
        .submit_and_watch(
            ControllerSpec::queue(instant_task(format!("warm-{label}")))
                .with_slot(format!("warm-slot-{label}")),
        )
        .await
        .expect("controller warmup intake failed");
    expect_completed(waiter).await;
}

async fn start_held_owner(
    handle: &SupervisorHandle,
    slot: &str,
    name: &str,
) -> (taskvisor::TaskId, TaskWaiter, Arc<AsyncFlag>) {
    let started = AsyncFlag::new();
    let release = AsyncFlag::new();
    let (_, waiter) = handle
        .submit_and_watch(
            ControllerSpec::queue(held_owner_task(
                name,
                Arc::clone(&started),
                Arc::clone(&release),
            ))
            .with_slot(slot),
        )
        .await
        .expect("held owner intake failed");
    let id = waiter.id();
    expect_within("the held owner to start", started.wait()).await;
    (id, waiter, release)
}

fn bench_cold_first_try_submit(c: &mut Criterion) {
    print_suite_header("controller");
    let mut group = c.benchmark_group(COLD_INTAKE.group_id);
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(10));
    group.throughput(Throughput::Elements(1));

    for &(rt_name, rt_fn) in &RUNTIMES {
        group.bench_function(rt_name, |b| {
            record_case(COLD_INTAKE, rt_name, None);
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for i in 0..iters {
                    let rt = rt_fn();
                    total += rt.block_on(async {
                        let supervisor = Supervisor::builder(bench_config())
                            .with_controller(ControllerConfig::default())
                            .build();
                        let handle = supervisor.serve().expect("runtime startup");
                        let request = ControllerSpec::queue(instant_task(format!("cold-{i}")))
                            .with_slot("cold-slot");

                        let start = Instant::now();
                        let id = handle
                            .try_submit(request)
                            .expect("first controller intake failed");
                        let elapsed = start.elapsed();
                        black_box(id);

                        handle.shutdown().await.expect("shutdown failed");
                        elapsed
                    });
                }
                total
            });
        });
    }
    group.finish();
}

fn bench_steady_try_submit(c: &mut Criterion) {
    let mut group = c.benchmark_group(STEADY_INTAKE.group_id);
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for count in [100usize, 500, 1000] {
            group.throughput(Throughput::Elements(count as u64));
            group.bench_with_input(
                BenchmarkId::new(rt_name, format!("{count}_accepted_submissions")),
                &count,
                |b, &count| {
                    record_case(
                        STEADY_INTAKE,
                        rt_name,
                        Some(format!("{count}_accepted_submissions")),
                    );
                    b.iter_custom(|iters| {
                        let mut total = Duration::ZERO;
                        for iteration in 0..iters {
                            let rt = rt_fn();
                            total += rt.block_on(async {
                                let queue_capacity = NonZeroUsize::new(count + 64).unwrap();
                                let supervisor = Supervisor::builder(bench_config())
                                    .with_controller(
                                        ControllerConfig::default()
                                            .with_queue_capacity(queue_capacity),
                                    )
                                    .build();
                                let handle = supervisor.serve().expect("runtime startup");
                                warm_controller(&handle, &format!("intake-{iteration}")).await;
                                let requests: Vec<_> = (0..count)
                                    .map(|i| {
                                        ControllerSpec::drop_if_running(instant_task(format!(
                                            "intake-{iteration}-{i}"
                                        )))
                                        .with_slot("intake-slot")
                                    })
                                    .collect();

                                let start = Instant::now();
                                for request in requests {
                                    let id = handle
                                        .try_submit(request)
                                        .expect("steady try_submit intake failed");
                                    black_box(id);
                                }
                                let elapsed = start.elapsed();

                                handle.shutdown().await.expect("shutdown failed");
                                elapsed
                            });
                        }
                        total
                    });
                },
            );
        }
    }
    group.finish();
}

fn bench_drop_busy_rejection(c: &mut Criterion) {
    let mut group = c.benchmark_group(DROP_REJECTION.group_id);
    group.sample_size(15);
    group.measurement_time(Duration::from_secs(12));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for count in [5usize, 20, 50] {
            group.throughput(Throughput::Elements(count as u64));
            group.bench_with_input(
                BenchmarkId::new(rt_name, format!("{count}_verified_rejections")),
                &count,
                |b, &count| {
                    record_case(
                        DROP_REJECTION,
                        rt_name,
                        Some(format!("{count}_verified_rejections")),
                    );
                    b.iter_custom(|iters| {
                        let mut total = Duration::ZERO;
                        for iteration in 0..iters {
                            let rt = rt_fn();
                            total += rt.block_on(async {
                                let supervisor = Supervisor::builder(bench_config())
                                    .with_controller(ControllerConfig::default())
                                    .build();
                                let handle = supervisor.serve().expect("runtime startup");
                                let (owner_id, owner_waiter, release) = start_held_owner(
                                    &handle,
                                    "drop-slot",
                                    &format!("drop-owner-{iteration}"),
                                )
                                .await;
                                let requests: Vec<_> = (0..count)
                                    .map(|i| {
                                        ControllerSpec::drop_if_running(instant_task(format!(
                                            "drop-{iteration}-{i}"
                                        )))
                                        .with_slot("drop-slot")
                                    })
                                    .collect();

                                let start = Instant::now();
                                let mut waiters = Vec::with_capacity(count);
                                for request in requests {
                                    let (_, waiter) = handle
                                        .submit_and_watch(request)
                                        .await
                                        .expect("DropIfRunning intake failed");
                                    waiters.push(waiter);
                                }
                                for waiter in waiters {
                                    expect_rejected(waiter, RejectionKind::SlotBusy).await;
                                }
                                let elapsed = start.elapsed();

                                release.mark();
                                assert!(
                                    handle.cancel(owner_id).await.expect("owner cancel failed"),
                                    "benchmark must claim the held owner"
                                );
                                expect_canceled(owner_waiter).await;
                                handle.shutdown().await.expect("shutdown failed");
                                elapsed
                            });
                        }
                        total
                    });
                },
            );
        }
    }
    group.finish();
}

fn bench_replace_busy_placement(c: &mut Criterion) {
    let mut group = c.benchmark_group(REPLACE_PLACEMENT.group_id);
    group.sample_size(15);
    group.measurement_time(Duration::from_secs(12));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for count in [5usize, 20, 50] {
            group.throughput(Throughput::Elements(count as u64));
            group.bench_with_input(
                BenchmarkId::new(rt_name, format!("{count}_processed_replacements")),
                &count,
                |b, &count| {
                    record_case(
                        REPLACE_PLACEMENT,
                        rt_name,
                        Some(format!("{count}_processed_replacements")),
                    );
                    b.iter_custom(|iters| {
                        let mut total = Duration::ZERO;
                        for iteration in 0..iters {
                            let rt = rt_fn();
                            total += rt.block_on(async {
                                let supervisor = Supervisor::builder(bench_config())
                                    .with_controller(ControllerConfig::default())
                                    .build();
                                let handle = supervisor.serve().expect("runtime startup");
                                let (_, owner_waiter, release) = start_held_owner(
                                    &handle,
                                    "replace-slot",
                                    &format!("replace-owner-{iteration}"),
                                )
                                .await;
                                let requests: Vec<_> = (0..count)
                                    .map(|i| {
                                        ControllerSpec::replace(instant_task(format!(
                                            "replace-{iteration}-{i}"
                                        )))
                                        .with_slot("replace-slot")
                                    })
                                    .collect();

                                let start = Instant::now();
                                let mut waiters = Vec::with_capacity(count);
                                for request in requests {
                                    let (_, waiter) = handle
                                        .submit_and_watch(request)
                                        .await
                                        .expect("Replace intake failed");
                                    waiters.push(waiter);
                                }
                                let newest = waiters.pop().expect("replacement batch is non-empty");
                                for waiter in waiters {
                                    expect_rejected(waiter, RejectionKind::SupersededByReplace)
                                        .await;
                                }
                                let elapsed = start.elapsed();

                                release.mark();
                                expect_canceled(owner_waiter).await;
                                expect_completed(newest).await;
                                handle.shutdown().await.expect("shutdown failed");
                                elapsed
                            });
                        }
                        total
                    });
                },
            );
        }
    }
    group.finish();
}

fn bench_queue_one_slot(c: &mut Criterion) {
    let mut group = c.benchmark_group(QUEUE_ONE.group_id);
    group.sample_size(15);
    group.measurement_time(Duration::from_secs(12));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for count in [5usize, 20, 50] {
            group.throughput(Throughput::Elements(count as u64));
            group.bench_with_input(
                BenchmarkId::new(rt_name, format!("{count}_completed_tasks")),
                &count,
                |b, &count| {
                    record_case(QUEUE_ONE, rt_name, Some(format!("{count}_completed_tasks")));
                    b.iter_custom(|iters| {
                        let mut total = Duration::ZERO;
                        for iteration in 0..iters {
                            let rt = rt_fn();
                            total += rt.block_on(async {
                                let supervisor = Supervisor::builder(bench_config())
                                    .with_controller(ControllerConfig::default())
                                    .build();
                                let handle = supervisor.serve().expect("runtime startup");
                                warm_controller(&handle, &format!("queue-{iteration}")).await;
                                let requests: Vec<_> = (0..count)
                                    .map(|i| {
                                        ControllerSpec::queue(instant_task(format!(
                                            "queue-{iteration}-{i}"
                                        )))
                                        .with_slot("queue-slot")
                                    })
                                    .collect();

                                let start = Instant::now();
                                let mut waiters = Vec::with_capacity(count);
                                for request in requests {
                                    let (_, waiter) = handle
                                        .submit_and_watch(request)
                                        .await
                                        .expect("Queue intake failed");
                                    waiters.push(waiter);
                                }
                                for waiter in waiters {
                                    expect_completed(waiter).await;
                                }
                                let elapsed = start.elapsed();

                                handle.shutdown().await.expect("shutdown failed");
                                elapsed
                            });
                        }
                        total
                    });
                },
            );
        }
    }
    group.finish();
}

fn bench_queue_eight_slots(c: &mut Criterion) {
    const SLOTS: usize = 8;

    let mut group = c.benchmark_group(QUEUE_EIGHT.group_id);
    group.sample_size(15);
    group.measurement_time(Duration::from_secs(12));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for count in [8usize, 32, 64] {
            group.throughput(Throughput::Elements(count as u64));
            group.bench_with_input(
                BenchmarkId::new(rt_name, format!("{count}_completed_tasks")),
                &count,
                |b, &count| {
                    record_case(
                        QUEUE_EIGHT,
                        rt_name,
                        Some(format!("{count}_completed_tasks")),
                    );
                    b.iter_custom(|iters| {
                        let mut total = Duration::ZERO;
                        for iteration in 0..iters {
                            let rt = rt_fn();
                            total += rt.block_on(async {
                                let supervisor = Supervisor::builder(bench_config())
                                    .with_controller(ControllerConfig::default())
                                    .build();
                                let handle = supervisor.serve().expect("runtime startup");
                                warm_controller(&handle, &format!("multi-{iteration}")).await;
                                let requests: Vec<_> = (0..count)
                                    .map(|i| {
                                        ControllerSpec::queue(instant_task(format!(
                                            "multi-{iteration}-{i}"
                                        )))
                                        .with_slot(format!("multi-slot-{}", i % SLOTS))
                                    })
                                    .collect();

                                let start = Instant::now();
                                let mut waiters = Vec::with_capacity(count);
                                for request in requests {
                                    let (_, waiter) = handle
                                        .submit_and_watch(request)
                                        .await
                                        .expect("multi-slot Queue intake failed");
                                    waiters.push(waiter);
                                }
                                for waiter in waiters {
                                    expect_completed(waiter).await;
                                }
                                let elapsed = start.elapsed();

                                handle.shutdown().await.expect("shutdown failed");
                                elapsed
                            });
                        }
                        total
                    });
                },
            );
        }
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_cold_first_try_submit,
    bench_steady_try_submit,
    bench_drop_busy_rejection,
    bench_replace_busy_placement,
    bench_queue_one_slot,
    bench_queue_eight_slots,
);

fn main() {
    support::benchmark_main("controller", benches);
}
