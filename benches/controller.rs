//! # Controller admission and lifecycle benchmarks
//!
//! Separates first-use startup, caller-side intake, verified policy decisions, and controller-managed task outcomes.
//! Steady cases reuse a warmed runtime and supervisor; retained ownership drains outside each measured batch.
//! Each result is checked at its advertised boundary. Intake acceptance does not imply successful task completion.
//!
//! Run with `cargo bench --bench controller --features controller`.

mod support;

use std::hint::black_box;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use taskvisor::{
    BackoffPolicy, ControllerConfig, ControllerSpec, RejectionKind, RestartPolicy, SlotStatusKind,
    Supervisor, SupervisorHandle, TaskContext, TaskFn, TaskId, TaskOutcome, TaskRef, TaskSpec,
    TaskWaiter,
};

use support::fixtures::{
    self, AsyncFlag, ProducerGate, RUNTIMES, RtFactory, bench_config, expect_canceled,
    expect_completed, expect_within, instant_task, wait_for_ownership,
};
use support::{CaseFamily, print_suite_header, record_case};

const COLD_INTAKE: CaseFamily = CaseFamily::intake(
    "controller/cold/first_try_submit",
    "COLD FIRST TRY_SUBMIT",
    "accepted submission",
    "accepted submissions",
    "first caller-side try_submit on a fresh served supervisor, including lazy cleanup-worker startup",
    "Supervisor/controller startup, request construction, controller decision, task outcome, ownership drain, shutdown, and Tokio runtime construction",
);

const STEADY_INTAKE: CaseFamily = CaseFamily::intake(
    "controller/reused/intake_try_submit",
    "STEADY TRY_SUBMIT BURST",
    "accepted submission",
    "accepted submissions",
    "64 caller-side try_submit acceptances on a reused supervisor; the multi-thread controller can consume concurrently",
    "Supervisor/controller startup, named-slot warmup, request construction, waiting for controller decisions and outcomes, post-batch ownership drain, shutdown, and Tokio runtime construction",
);

const CONCURRENT_INTAKE: CaseFamily = CaseFamily::intake(
    "controller/reused/concurrent_intake_try_submit",
    "CONCURRENT TRY_SUBMIT PRODUCERS",
    "accepted submission",
    "accepted submissions",
    "synchronized release of 1, 2, 4, or 8 producer tasks through 64 caller-side try_submit acceptances on one reused multi-thread supervisor",
    "Supervisor/controller startup, named-slot warmup, request construction, producer task creation and readiness, controller decisions and outcomes, post-batch ownership drain, shutdown, and Tokio runtime construction",
);

const DROP_REJECTION: CaseFamily = CaseFamily::policy(
    "controller/reused/drop_busy_rejection",
    "DROP_IF_RUNNING · BUSY SLOT",
    "verified rejection",
    "verified rejections",
    "32 watched submissions through verified SlotBusy outcomes against one held owner on a reused supervisor",
    "Supervisor/controller startup, named-slot warmup, held-owner setup/release/cleanup, request construction, post-batch ownership drain, shutdown, and Tokio runtime construction",
);

const REPLACE_PLACEMENT: CaseFamily = CaseFamily::policy(
    "controller/reused/replace_busy_placement",
    "REPLACE · BUSY SLOT",
    "processed replacement",
    "processed replacements",
    "32 watched Replace submissions through 31 SupersededByReplace outcomes after the newest request has replaced the queue head",
    "Supervisor/controller startup, named-slot warmup, held-owner setup/release, request construction, retention snapshot check, newest task completion, post-batch ownership drain, shutdown, and Tokio runtime construction",
);

const QUEUE_ONE: CaseFamily = CaseFamily::lifecycle(
    "controller/reused/queue_one_slot",
    "QUEUE · ONE SLOT",
    "completed task",
    "completed tasks",
    "32 watched submissions through Completed outcomes in one slot on a reused current-thread runtime and supervisor",
    "Supervisor/controller startup, named-slot warmup, request construction, post-outcome ownership drain and slot reset, shutdown, and Tokio runtime construction",
);

const QUEUE_EIGHT: CaseFamily = CaseFamily::lifecycle(
    "controller/reused/queue_eight_slots",
    "QUEUE · EIGHT SLOTS",
    "completed task",
    "completed tasks",
    "64 watched submissions through Completed outcomes across eight named slots on a reused runtime and supervisor after warming those admission paths",
    "Supervisor/controller startup, named-slot warmup, request construction, post-outcome ownership drain and slot reset, shutdown, and Tokio runtime construction",
);

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

async fn expect_rejected(waiter: TaskWaiter, expected: RejectionKind) {
    let outcome = expect_within("a controller rejection", waiter.wait())
        .await
        .expect("submission outcome channel closed");
    assert!(
        matches!(outcome, TaskOutcome::Rejected { kind, .. } if kind == expected),
        "expected {expected:?}, got {outcome:?}"
    );
}

async fn drain_controller(handle: &SupervisorHandle) {
    // An outcome does not imply retained-value destruction or controller slot GC.
    // With no new submissions, both observations must settle before the next batch.
    wait_for_ownership(handle, 0).await;
    expect_within("controller slots to become idle", async {
        loop {
            let snapshot = handle
                .controller_snapshot()
                .await
                .expect("benchmark controller must exist");
            if snapshot.slots.iter().all(|slot| {
                slot.status == SlotStatusKind::Idle
                    && slot.owner_id.is_none()
                    && slot.queue_depth == 0
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
}

async fn warm_controller_slots(handle: &SupervisorHandle, slots: &[&str]) {
    for slot in slots {
        let (_, waiter) = handle
            .submit_and_watch(
                ControllerSpec::queue(instant_task(format!("warm-{slot}"))).with_slot(*slot),
            )
            .await
            .expect("controller warmup intake failed");
        expect_completed(waiter).await;
    }
    // Idle slots may be collected. Warm the same admission paths and wait for
    // their previous owners to leave instead of preserving a stale slot owner.
    drain_controller(handle).await;
}

async fn start_held_owner(
    handle: &SupervisorHandle,
    slot: &str,
    name: &str,
) -> (TaskId, TaskWaiter, Arc<AsyncFlag>) {
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
    expect_within("the held owner to reach the Running slot state", async {
        loop {
            let snapshot = handle
                .controller_snapshot()
                .await
                .expect("benchmark controller must exist");
            if snapshot.slot(slot).is_some_and(|view| {
                view.owner_id == Some(id)
                    && view.status == SlotStatusKind::Running
                    && view.queue_depth == 0
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    wait_for_ownership(handle, 1).await;
    (id, waiter, release)
}

fn bench_cold_first_try_submit(c: &mut Criterion) {
    print_suite_header("controller");
    let mut group = c.benchmark_group(COLD_INTAKE.group_id);
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

                        drain_controller(&handle).await;
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
    const COUNT: usize = 64;
    const VALUE: &str = "64_accepted_submissions";

    let mut group = c.benchmark_group(STEADY_INTAKE.group_id);
    group.throughput(Throughput::Elements(COUNT as u64));

    for &(rt_name, rt_fn) in &RUNTIMES {
        group.bench_function(BenchmarkId::new(rt_name, VALUE), |b| {
            record_case(STEADY_INTAKE, rt_name, Some(VALUE.to_owned()));
            b.iter_custom(|iters| {
                let rt = rt_fn();
                rt.block_on(async {
                    let supervisor = Supervisor::builder(bench_config())
                        .with_controller(
                            ControllerConfig::default()
                                .with_queue_capacity(NonZeroUsize::new(COUNT).unwrap()),
                        )
                        .build();
                    let handle = supervisor.serve().expect("runtime startup");
                    warm_controller_slots(&handle, &["intake-slot"]).await;

                    let mut total = Duration::ZERO;
                    for iteration in 0..iters {
                        let mut requests: Vec<_> = (0..COUNT)
                            .map(|i| {
                                ControllerSpec::drop_if_running(instant_task(format!(
                                    "intake-{iteration}-{i}"
                                )))
                                .with_slot("intake-slot")
                            })
                            .collect();

                        let start = Instant::now();
                        for request in requests.drain(..) {
                            let id = handle
                                .try_submit(request)
                                .expect("steady try_submit intake failed");
                            black_box(id);
                        }
                        total += start.elapsed();

                        // These units are accepted submissions, not successful tasks.
                        // Either policy rejection or completion must release ownership.
                        drain_controller(&handle).await;
                    }
                    handle.shutdown().await.expect("shutdown failed");
                    total
                })
            });
        });
    }
    group.finish();
}

fn bench_concurrent_try_submit(c: &mut Criterion) {
    const COUNT: usize = 64;

    let mut group = c.benchmark_group(CONCURRENT_INTAKE.group_id);
    group.throughput(Throughput::Elements(COUNT as u64));

    for producers in [1usize, 2, 4, 8] {
        let rt_name = "multi_thread";
        let parameter = format!("{COUNT}_accepted_submissions_{producers}_producers");
        group.bench_function(BenchmarkId::new(rt_name, &parameter), |b| {
            record_case(CONCURRENT_INTAKE, rt_name, Some(parameter.clone()));
            b.iter_custom(|iters| {
                let rt = fixtures::rt_multi_thread();
                rt.block_on(async {
                    let supervisor = Supervisor::builder(bench_config())
                        .with_controller(
                            ControllerConfig::default()
                                .with_queue_capacity(NonZeroUsize::new(COUNT).unwrap()),
                        )
                        .build();
                    let handle = supervisor.serve().expect("runtime startup");
                    warm_controller_slots(&handle, &["concurrent-intake-slot"]).await;

                    let mut total = Duration::ZERO;
                    for iteration in 0..iters {
                        let per_producer = COUNT / producers;
                        assert_eq!(per_producer * producers, COUNT);
                        let gate = ProducerGate::new(producers);
                        let mut joins = Vec::with_capacity(producers);

                        for producer in 0..producers {
                            let requests: Vec<_> = (0..per_producer)
                                .map(|i| {
                                    ControllerSpec::drop_if_running(instant_task(format!(
                                        "concurrent-intake-{iteration}-{producer}-{i}"
                                    )))
                                    .with_slot("concurrent-intake-slot")
                                })
                                .collect();
                            let producer_handle = handle.clone();
                            let producer_gate = Arc::clone(&gate);
                            joins.push(tokio::spawn(async move {
                                producer_gate.arrive_and_wait().await;
                                requests
                                    .into_iter()
                                    .map(|request| {
                                        producer_handle
                                            .try_submit(request)
                                            .expect("concurrent try_submit intake failed")
                                    })
                                    .collect::<Vec<_>>()
                            }));
                        }

                        gate.wait_until_ready().await;
                        let start = Instant::now();
                        gate.release();
                        let mut accepted = 0usize;
                        for join in joins {
                            let ids = join.await.expect("concurrent producer task failed");
                            accepted += ids.len();
                            black_box(ids);
                        }
                        total += start.elapsed();
                        assert_eq!(accepted, COUNT);

                        drain_controller(&handle).await;
                    }

                    handle.shutdown().await.expect("shutdown failed");
                    total
                })
            });
        });
    }
    group.finish();
}

fn bench_drop_busy_rejection(c: &mut Criterion) {
    const COUNT: usize = 32;
    const VALUE: &str = "32_verified_rejections";

    let mut group = c.benchmark_group(DROP_REJECTION.group_id);
    group.throughput(Throughput::Elements(COUNT as u64));

    for &(rt_name, rt_fn) in &RUNTIMES {
        group.bench_function(BenchmarkId::new(rt_name, VALUE), |b| {
            record_case(DROP_REJECTION, rt_name, Some(VALUE.to_owned()));
            b.iter_custom(|iters| {
                let rt = rt_fn();
                rt.block_on(async {
                    let supervisor = Supervisor::builder(bench_config())
                        .with_controller(ControllerConfig::default())
                        .build();
                    let handle = supervisor.serve().expect("runtime startup");
                    warm_controller_slots(&handle, &["drop-slot"]).await;
                    let (owner_id, owner_waiter, release) =
                        start_held_owner(&handle, "drop-slot", "drop-owner").await;

                    let mut total = Duration::ZERO;
                    for iteration in 0..iters {
                        let mut requests: Vec<_> = (0..COUNT)
                            .map(|i| {
                                ControllerSpec::drop_if_running(instant_task(format!(
                                    "drop-{iteration}-{i}"
                                )))
                                .with_slot("drop-slot")
                            })
                            .collect();
                        let mut waiters = Vec::with_capacity(COUNT);

                        let start = Instant::now();
                        for request in requests.drain(..) {
                            let (_, waiter) = handle
                                .submit_and_watch(request)
                                .await
                                .expect("DropIfRunning intake failed");
                            waiters.push(waiter);
                        }
                        for waiter in waiters.drain(..) {
                            expect_rejected(waiter, RejectionKind::SlotBusy).await;
                        }
                        total += start.elapsed();

                        wait_for_ownership(&handle, 1).await;
                    }

                    release.mark();
                    assert!(
                        handle.cancel(owner_id).await.expect("owner cancel failed"),
                        "benchmark must claim the held owner"
                    );
                    expect_canceled(owner_waiter).await;
                    drain_controller(&handle).await;
                    handle.shutdown().await.expect("shutdown failed");
                    total
                })
            });
        });
    }
    group.finish();
}

fn bench_replace_busy_placement(c: &mut Criterion) {
    const COUNT: usize = 32;
    const VALUE: &str = "32_processed_replacements";

    let mut group = c.benchmark_group(REPLACE_PLACEMENT.group_id);
    group.throughput(Throughput::Elements(COUNT as u64));

    for &(rt_name, rt_fn) in &RUNTIMES {
        group.bench_function(BenchmarkId::new(rt_name, VALUE), |b| {
            record_case(REPLACE_PLACEMENT, rt_name, Some(VALUE.to_owned()));
            b.iter_custom(|iters| {
                let rt = rt_fn();
                rt.block_on(async {
                    let supervisor = Supervisor::builder(bench_config())
                        .with_controller(ControllerConfig::default())
                        .build();
                    let handle = supervisor.serve().expect("runtime startup");
                    warm_controller_slots(&handle, &["replace-slot"]).await;

                    let mut total = Duration::ZERO;
                    for iteration in 0..iters {
                        let (owner_id, owner_waiter, release) = start_held_owner(
                            &handle,
                            "replace-slot",
                            &format!("replace-owner-{iteration}"),
                        )
                        .await;
                        let mut requests: Vec<_> = (0..COUNT)
                            .map(|i| {
                                ControllerSpec::replace(instant_task(format!(
                                    "replace-{iteration}-{i}"
                                )))
                                .with_slot("replace-slot")
                            })
                            .collect();
                        let mut waiters = Vec::with_capacity(COUNT);

                        let start = Instant::now();
                        for request in requests.drain(..) {
                            let (_, waiter) = handle
                                .submit_and_watch(request)
                                .await
                                .expect("Replace intake failed");
                            waiters.push(waiter);
                        }
                        let newest = waiters.pop().expect("replacement batch is non-empty");
                        for waiter in waiters.drain(..) {
                            expect_rejected(waiter, RejectionKind::SupersededByReplace).await;
                        }
                        total += start.elapsed();

                        // Head replacement happens before the displaced waiter is resolved.
                        // All 31 rejections therefore prove that the newest head was installed.
                        // Check the held-owner/retained-head state without timing a query.
                        let snapshot = handle
                            .controller_snapshot()
                            .await
                            .expect("benchmark controller must exist");
                        let slot = snapshot
                            .slot("replace-slot")
                            .expect("the held owner must retain its slot");
                        assert_eq!(slot.owner_id, Some(owner_id));
                        assert_eq!(slot.status, SlotStatusKind::Terminating);
                        assert_eq!(slot.queue_depth, 1);

                        release.mark();
                        expect_canceled(owner_waiter).await;
                        expect_completed(newest).await;
                        drain_controller(&handle).await;
                    }
                    handle.shutdown().await.expect("shutdown failed");
                    total
                })
            });
        });
    }
    group.finish();
}

fn bench_queue_workload(
    c: &mut Criterion,
    family: CaseFamily,
    count: usize,
    slots: &[&str],
    runtimes: &[(&str, RtFactory)],
) {
    let value = format!("{count}_completed_tasks");
    let mut group = c.benchmark_group(family.group_id);
    group.throughput(Throughput::Elements(count as u64));

    for &(rt_name, rt_fn) in runtimes {
        group.bench_function(BenchmarkId::new(rt_name, &value), |b| {
            record_case(family, rt_name, Some(value.clone()));
            b.iter_custom(|iters| {
                let rt = rt_fn();
                rt.block_on(async {
                    let supervisor = Supervisor::builder(bench_config())
                        .with_controller(ControllerConfig::default())
                        .build();
                    let handle = supervisor.serve().expect("runtime startup");
                    warm_controller_slots(&handle, slots).await;

                    let mut total = Duration::ZERO;
                    for iteration in 0..iters {
                        let mut requests: Vec<_> = (0..count)
                            .map(|i| {
                                ControllerSpec::queue(instant_task(format!(
                                    "queue-{iteration}-{i}"
                                )))
                                .with_slot(slots[i % slots.len()])
                            })
                            .collect();
                        let mut waiters = Vec::with_capacity(count);

                        let start = Instant::now();
                        for request in requests.drain(..) {
                            let (_, waiter) = handle
                                .submit_and_watch(request)
                                .await
                                .expect("Queue intake failed");
                            waiters.push(waiter);
                        }
                        for waiter in waiters.drain(..) {
                            expect_completed(waiter).await;
                        }
                        total += start.elapsed();

                        drain_controller(&handle).await;
                    }
                    handle.shutdown().await.expect("shutdown failed");
                    total
                })
            });
        });
    }
    group.finish();
}

fn bench_queue_one_slot(c: &mut Criterion) {
    bench_queue_workload(
        c,
        QUEUE_ONE,
        32,
        &["queue-slot"],
        &[("current_thread", fixtures::rt_current_thread)],
    );
}

fn bench_queue_eight_slots(c: &mut Criterion) {
    bench_queue_workload(
        c,
        QUEUE_EIGHT,
        64,
        &[
            "multi-slot-0",
            "multi-slot-1",
            "multi-slot-2",
            "multi-slot-3",
            "multi-slot-4",
            "multi-slot-5",
            "multi-slot-6",
            "multi-slot-7",
        ],
        &RUNTIMES,
    );
}

criterion_group! {
    name = benches;
    config = fixtures::criterion();
    targets =
        bench_cold_first_try_submit,
        bench_steady_try_submit,
        bench_concurrent_try_submit,
        bench_drop_busy_rejection,
        bench_replace_busy_placement,
        bench_queue_one_slot,
        bench_queue_eight_slots,
}

fn main() {
    support::benchmark_main("controller", benches);
}
