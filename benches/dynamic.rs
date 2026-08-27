//! Dynamic admission, cancellation, registry queries, and ownership recovery.
//!
//! Cases keep setup and physical ownership reset outside their stated boundaries.

mod support;

use std::future::{Future, poll_fn};
use std::hint::black_box;
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use futures_util::future::join_all;
use taskvisor::{
    BoxTaskFuture, Supervisor, SupervisorHandle, Task, TaskContext, TaskFn, TaskRef, TaskSpec,
};

use support::fixtures::{
    self, AsyncFlag, BlockingGate, RUNTIMES, ReleaseOnDrop, bench_config, expect_canceled,
    expect_completed, expect_within, instant_task, wait_for_ownership, warm_runtime,
};
use support::{CaseFamily, print_suite_header, record_case};

const BURST_SIZE: usize = 32;

const ROOT_SEQUENTIAL_ADD: CaseFamily = CaseFamily::intake(
    "dynamic/latency/sequential_registry_add_root",
    "SEQUENTIAL REGISTRY ADD · ROOT CALLER",
    "accepted add",
    "accepted adds",
    "32 serialized add calls from the Runtime::block_on root through authoritative registry decisions on a warmed supervisor with free ownership and registry capacity",
    "TaskSpec construction, Supervisor startup and warmup, result validation, cancellation, physical ownership reset, shutdown, and Tokio runtime construction",
);

const WORKER_SEQUENTIAL_ADD: CaseFamily = CaseFamily::intake(
    "dynamic/latency/sequential_registry_add_worker",
    "SEQUENTIAL REGISTRY ADD · SPAWNED CALLER",
    "accepted add",
    "accepted adds",
    "32 serialized add calls from one spawned Tokio task through authoritative registry decisions on a warmed supervisor with free ownership and registry capacity",
    "TaskSpec construction, spawned-task scheduling before its internal timer, root-side JoinHandle polling, result validation, cancellation, physical ownership reset, shutdown, and Tokio runtime construction",
);

const WORKER_PIPELINED_ADD: CaseFamily = CaseFamily::intake(
    "dynamic/throughput/pipelined_registry_add_worker",
    "PIPELINED REGISTRY ADD · SPAWNED CALLER",
    "accepted add",
    "accepted adds",
    "concurrent polling of 32 prebuilt add futures from one spawned Tokio task through all authoritative registry decisions on a warmed supervisor with free ownership and registry capacity",
    "TaskSpec and add-future construction, spawned-task scheduling before its internal timer, root-side JoinHandle polling, result validation, cancellation, physical ownership reset, shutdown, and Tokio runtime construction",
);

const CANCEL_STARTED: CaseFamily = CaseFamily::lifecycle(
    "dynamic/steady/cancel_started",
    "CANCEL A STARTED COOPERATIVE TASK",
    "canceled task",
    "canceled tasks",
    "cancel of an already-started cooperative task through terminal cancel confirmation and a verified Canceled outcome",
    "TaskSpec construction, Supervisor startup and warmup, admission, the started handshake, physical ownership reset, shutdown, and Tokio runtime construction",
)
.without_lifecycle_interpretation();

const LIST: CaseFamily = CaseFamily::query(
    "dynamic/steady/list_held_tasks",
    "HELD-TASK REGISTRY SNAPSHOT",
    "snapshot call",
    "snapshot calls",
    "one complete registry snapshot while the named number of cooperative tasks remains registered",
    "Supervisor startup and warmup, registry prepopulation, result validation, cancellation, physical ownership reset, shutdown, and Tokio runtime construction",
);

const OWNERSHIP_RELEASE: CaseFamily = CaseFamily::intake(
    "dynamic/steady/ownership_release_to_admission",
    "OWNERSHIP RELEASE TO WATCHED ADMISSION",
    "accepted watched add",
    "accepted watched adds",
    "release of a completed task's blocked final Drop through acceptance of one already-parked watched add at ownership capacity one",
    "Supervisor startup and warmup, holder completion, gate-entry and parked-waiter checks, the next task's outcome, physical ownership reset, shutdown, and Tokio runtime construction",
);

fn cooperative_task(name: impl Into<Arc<str>>, started: Option<Arc<AsyncFlag>>) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(move |ctx: TaskContext| {
        let started = started.clone();
        async move {
            if let Some(started) = started {
                started.mark();
            }
            ctx.cancelled().await;
            Ok(())
        }
    });
    TaskSpec::once(name, task)
}

async fn cancel_held(handle: &SupervisorHandle, ids: impl IntoIterator<Item = taskvisor::TaskId>) {
    for id in ids {
        assert!(
            expect_within("held-task cancellation", handle.cancel(id).execute())
                .await
                .expect("held-task cancellation failed"),
            "benchmark must claim each held task"
        );
    }
    wait_for_ownership(handle, 0).await;
}

struct RetainedFinalDrop {
    gate: Arc<BlockingGate>,
}

impl Task for RetainedFinalDrop {
    fn spawn(&self, _ctx: TaskContext) -> BoxTaskFuture {
        Box::pin(async { Ok(()) })
    }
}

impl Drop for RetainedFinalDrop {
    fn drop(&mut self) {
        self.gate.wait();
    }
}

fn bench_registry_add(c: &mut Criterion) {
    print_suite_header("dynamic management");
    let mut group = c.benchmark_group(ROOT_SEQUENTIAL_ADD.group_id);
    group.throughput(Throughput::Elements(BURST_SIZE as u64));

    for &(rt_name, rt_fn) in &RUNTIMES {
        group.bench_function(
            BenchmarkId::new(rt_name, format!("{BURST_SIZE}_adds")),
            |b| {
                record_case(
                    ROOT_SEQUENTIAL_ADD,
                    rt_name,
                    Some(format!("{BURST_SIZE}_adds")),
                );
                b.iter_custom(|iters| {
                    let rt = rt_fn();
                    rt.block_on(async {
                        let supervisor = Supervisor::new(bench_config(), vec![]);
                        let handle = supervisor.serve().expect("runtime startup");
                        warm_runtime(&handle, 0).await;

                        let mut total = Duration::ZERO;
                        for _ in 0..iters {
                            let specs: Vec<_> = (0..BURST_SIZE)
                                .map(|i| cooperative_task(format!("burst-{i}"), None))
                                .collect();
                            let mut accepted = Vec::with_capacity(BURST_SIZE);
                            assert!(
                                handle
                                    .ownership_snapshot()
                                    .available
                                    .is_some_and(|available| available >= BURST_SIZE),
                                "the full burst must fit without waiting for task cleanup"
                            );

                            let elapsed = expect_within("root-caller sequential adds", async {
                                let start = Instant::now();
                                for spec in specs {
                                    accepted.push(handle.add(spec).execute().await);
                                }
                                start.elapsed()
                            })
                            .await;

                            let ids: Vec<_> = accepted
                                .into_iter()
                                .map(|result| result.expect("burst admission failed"))
                                .collect();
                            cancel_held(&handle, ids).await;
                            total += elapsed;
                        }

                        expect_within("runtime shutdown", handle.shutdown())
                            .await
                            .expect("runtime shutdown failed");
                        total
                    })
                });
            },
        );
    }
    group.finish();

    let mut group = c.benchmark_group(WORKER_SEQUENTIAL_ADD.group_id);
    group.throughput(Throughput::Elements(BURST_SIZE as u64));

    for &(rt_name, rt_fn) in &RUNTIMES {
        group.bench_function(
            BenchmarkId::new(rt_name, format!("{BURST_SIZE}_adds")),
            |b| {
                record_case(
                    WORKER_SEQUENTIAL_ADD,
                    rt_name,
                    Some(format!("{BURST_SIZE}_adds")),
                );
                b.iter_custom(|iters| {
                    let rt = rt_fn();
                    rt.block_on(async {
                        let supervisor = Supervisor::new(bench_config(), vec![]);
                        let handle = supervisor.serve().expect("runtime startup");
                        warm_runtime(&handle, 0).await;

                        let mut total = Duration::ZERO;
                        for _ in 0..iters {
                            let specs: Vec<_> = (0..BURST_SIZE)
                                .map(|i| cooperative_task(format!("worker-sequential-{i}"), None))
                                .collect();
                            assert!(
                                handle
                                    .ownership_snapshot()
                                    .available
                                    .is_some_and(|available| available >= BURST_SIZE),
                                "the full batch must fit without waiting for task cleanup"
                            );

                            let worker_handle = handle.clone();
                            let caller = tokio::spawn(async move {
                                let mut accepted = Vec::with_capacity(BURST_SIZE);
                                let start = Instant::now();
                                for spec in specs {
                                    accepted.push(worker_handle.add(spec).execute().await);
                                }
                                (accepted, start.elapsed())
                            });
                            let (accepted, elapsed) =
                                expect_within("spawned-caller sequential adds", caller)
                                    .await
                                    .expect("spawned add caller failed");

                            let ids: Vec<_> = accepted
                                .into_iter()
                                .map(|result| result.expect("sequential admission failed"))
                                .collect();
                            cancel_held(&handle, ids).await;
                            total += elapsed;
                        }

                        expect_within("runtime shutdown", handle.shutdown())
                            .await
                            .expect("runtime shutdown failed");
                        total
                    })
                });
            },
        );
    }
    group.finish();

    let mut group = c.benchmark_group(WORKER_PIPELINED_ADD.group_id);
    group.throughput(Throughput::Elements(BURST_SIZE as u64));

    for &(rt_name, rt_fn) in &RUNTIMES {
        group.bench_function(
            BenchmarkId::new(rt_name, format!("{BURST_SIZE}_adds")),
            |b| {
                record_case(
                    WORKER_PIPELINED_ADD,
                    rt_name,
                    Some(format!("{BURST_SIZE}_adds")),
                );
                b.iter_custom(|iters| {
                    let rt = rt_fn();
                    rt.block_on(async {
                        let supervisor = Supervisor::new(bench_config(), vec![]);
                        let handle = supervisor.serve().expect("runtime startup");
                        warm_runtime(&handle, 0).await;

                        let mut total = Duration::ZERO;
                        for _ in 0..iters {
                            let specs: Vec<_> = (0..BURST_SIZE)
                                .map(|i| cooperative_task(format!("worker-pipelined-{i}"), None))
                                .collect();
                            assert!(
                                handle
                                    .ownership_snapshot()
                                    .available
                                    .is_some_and(|available| available >= BURST_SIZE),
                                "the full batch must fit without waiting for task cleanup"
                            );

                            let worker_handle = handle.clone();
                            let caller = tokio::spawn(async move {
                                let additions = join_all(
                                    specs
                                        .into_iter()
                                        .map(|spec| worker_handle.add(spec).execute()),
                                );
                                let start = Instant::now();
                                let accepted = additions.await;
                                (accepted, start.elapsed())
                            });
                            let (accepted, elapsed) =
                                expect_within("spawned-caller pipelined adds", caller)
                                    .await
                                    .expect("spawned add caller failed");

                            let ids: Vec<_> = accepted
                                .into_iter()
                                .map(|result| result.expect("pipelined admission failed"))
                                .collect();
                            cancel_held(&handle, ids).await;
                            total += elapsed;
                        }

                        expect_within("runtime shutdown", handle.shutdown())
                            .await
                            .expect("runtime shutdown failed");
                        total
                    })
                });
            },
        );
    }
    group.finish();
}

fn bench_cancel_started(c: &mut Criterion) {
    let mut group = c.benchmark_group(CANCEL_STARTED.group_id);
    group.throughput(Throughput::Elements(1));

    for &(rt_name, rt_fn) in &RUNTIMES {
        group.bench_function(rt_name, |b| {
            record_case(CANCEL_STARTED, rt_name, None);
            b.iter_custom(|iters| {
                let rt = rt_fn();
                rt.block_on(async {
                    let supervisor = Supervisor::new(bench_config(), vec![]);
                    let handle = supervisor.serve().expect("runtime startup");
                    warm_runtime(&handle, 0).await;

                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let started = AsyncFlag::new();
                        let spec = cooperative_task("cancel-started", Some(Arc::clone(&started)));
                        let waiter = expect_within(
                            "cancel-case admission",
                            handle.add(spec).watch().execute(),
                        )
                        .await
                        .expect("cancel-case admission failed");
                        let id = waiter.id();
                        expect_within("the task to start", started.wait()).await;

                        let elapsed = expect_within("started-task cancellation", async {
                            let start = Instant::now();
                            let claimed = handle.cancel(id).execute().await.expect("cancel failed");
                            expect_canceled(waiter).await;
                            let elapsed = start.elapsed();
                            assert!(claimed, "benchmark must claim its started task");
                            elapsed
                        })
                        .await;
                        wait_for_ownership(&handle, 0).await;
                        total += elapsed;
                    }

                    expect_within("runtime shutdown", handle.shutdown())
                        .await
                        .expect("runtime shutdown failed");
                    total
                })
            });
        });
    }
    group.finish();
}

fn bench_list(c: &mut Criterion) {
    let mut group = c.benchmark_group(LIST.group_id);
    group.throughput(Throughput::Elements(1));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for count in [32, 256] {
            group.bench_with_input(
                BenchmarkId::new(rt_name, format!("{count}_held_tasks")),
                &count,
                |b, &count| {
                    record_case(LIST, rt_name, Some(format!("{count}_held_tasks")));
                    b.iter_custom(|iters| {
                        let rt = rt_fn();
                        rt.block_on(async {
                            let supervisor = Supervisor::new(bench_config(), vec![]);
                            let handle = supervisor.serve().expect("runtime startup");
                            warm_runtime(&handle, 0).await;
                            let mut ids = Vec::with_capacity(count);
                            for i in 0..count {
                                let spec = cooperative_task(format!("list-{i}"), None);
                                let id = expect_within(
                                    "snapshot prepopulation",
                                    handle.add(spec).execute(),
                                )
                                .await
                                .expect("snapshot prepopulation failed");
                                ids.push(id);
                            }
                            wait_for_ownership(&handle, count).await;

                            let mut total = Duration::ZERO;
                            for _ in 0..iters {
                                let (snapshot, elapsed) =
                                    expect_within("a registry snapshot", async {
                                        let start = Instant::now();
                                        let snapshot = handle.list().await;
                                        (snapshot, start.elapsed())
                                    })
                                    .await;
                                assert_eq!(
                                    snapshot.len(),
                                    count,
                                    "registry snapshot is incomplete"
                                );
                                black_box(snapshot);
                                total += elapsed;
                            }

                            cancel_held(&handle, ids).await;
                            expect_within("runtime shutdown", handle.shutdown())
                                .await
                                .expect("runtime shutdown failed");
                            total
                        })
                    });
                },
            );
        }
    }
    group.finish();
}

fn bench_ownership_release(c: &mut Criterion) {
    let mut group = c.benchmark_group(OWNERSHIP_RELEASE.group_id);
    group.throughput(Throughput::Elements(1));

    for &(rt_name, rt_fn) in &RUNTIMES {
        group.bench_function(rt_name, |b| {
            record_case(OWNERSHIP_RELEASE, rt_name, None);
            b.iter_custom(|iters| {
                let rt = rt_fn();
                rt.block_on(async {
                    let config = bench_config()
                        .try_with_ownership_capacity(1)
                        .expect("one ownership unit is valid");
                    let supervisor = Supervisor::new(config, vec![]);
                    let handle = supervisor.serve().expect("runtime startup");
                    warm_runtime(&handle, 0).await;

                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let gate = BlockingGate::new();
                        let _release_on_failure = ReleaseOnDrop::new(Arc::clone(&gate));
                        let task: TaskRef = Arc::new(RetainedFinalDrop {
                            gate: Arc::clone(&gate),
                        });
                        let holder = expect_within(
                            "cleanup-holder admission",
                            handle
                                .add(TaskSpec::once("retained-final-drop", task))
                                .watch()
                                .execute(),
                        )
                        .await
                        .expect("cleanup-holder admission failed");
                        expect_completed(holder).await;
                        gate.wait_until_blocked().await;
                        let blocked = handle.ownership_snapshot();
                        assert_eq!(blocked.available, Some(0));
                        assert_eq!(blocked.cleanup_running, 1);

                        let mut admission =
                            Box::pin(handle.add(instant_task("after-release")).watch().execute());
                        expect_within(
                            "ownership waiter registration",
                            poll_fn(|cx| {
                                assert!(
                                    admission.as_mut().poll(cx).is_pending(),
                                    "admission must wait for the retained ownership unit"
                                );
                                if handle.ownership_snapshot().waiters == 1 {
                                    Poll::Ready(())
                                } else {
                                    Poll::Pending
                                }
                            }),
                        )
                        .await;
                        gate.assert_blocked();

                        let (accepted, elapsed) =
                            expect_within("ownership release to admission", async {
                                let start = Instant::now();
                                gate.release();
                                let accepted = admission.as_mut().await;
                                (accepted, start.elapsed())
                            })
                            .await;
                        let waiter = accepted.expect("released ownership must admit the next task");
                        gate.assert_not_timed_out();
                        black_box(waiter.id());
                        expect_completed(waiter).await;
                        wait_for_ownership(&handle, 0).await;
                        total += elapsed;
                    }

                    expect_within("runtime shutdown", handle.shutdown())
                        .await
                        .expect("runtime shutdown failed");
                    total
                })
            });
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = fixtures::criterion();
    targets = bench_registry_add, bench_cancel_started, bench_list, bench_ownership_release
}

fn main() {
    support::benchmark_main("dynamic management", benches);
}
