//! # Dynamic management benchmarks
//!
//! Measures checked `SupervisorHandle` operations against a served supervisor. Steady cases
//! prewarm Taskvisor before timing. The cold churn case includes first ownership admission.
//!
//! Run with `cargo bench --bench dynamic`.

mod support;

use std::hint::black_box;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use taskvisor::TaskContext;
use tokio::runtime::Runtime;

use taskvisor::{
    BackoffPolicy, RestartPolicy, Supervisor, SupervisorConfig, SupervisorHandle, TaskFn,
    TaskOutcome, TaskRef, TaskSpec,
};

use support::{CaseFamily, print_suite_header, record_case};

const REGISTRY_ADD: CaseFamily = CaseFamily::intake(
    "dynamic/steady/sustained_registry_add",
    "SUSTAINED REGISTRY ADD",
    "accepted add",
    "accepted adds",
    "prewarmed add through ownership admission and registry acceptance under lifecycle backpressure",
    "Supervisor startup, warmup, TaskSpec construction, submitted-task completion, final drain, shutdown, and Tokio runtime construction",
);

const ADD_CANCEL: CaseFamily = CaseFamily::lifecycle(
    "dynamic/steady/add_cancel",
    "ADD + TERMINAL CANCEL",
    "management cycle",
    "management cycles",
    "prewarmed add through registry acceptance and terminal cancel confirmation",
    "Supervisor startup, warmup, TaskSpec construction, shutdown, and Tokio runtime construction",
)
.without_lifecycle_interpretation();

const LIST: CaseFamily = CaseFamily::query(
    "dynamic/steady/list_snapshot",
    "REGISTRY SNAPSHOT",
    "snapshot call",
    "snapshot calls",
    "one registry snapshot with the named registered-task count",
    "Supervisor startup, registry prepopulation, shutdown, and Tokio runtime construction",
);

const ADD_SHUTDOWN: CaseFamily = CaseFamily::lifecycle(
    "dynamic/cold/add_shutdown",
    "COLD ADD + SHUTDOWN",
    "cleaned task",
    "cleaned tasks",
    "first add through cleanup of the named batch during shutdown",
    "Supervisor construction/startup, TaskSpec values, and Tokio runtime construction",
)
.without_lifecycle_interpretation();

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
        .with_bus_capacity(std::num::NonZeroUsize::new(16384).unwrap())
        .with_grace(Duration::from_secs(5))
}

fn worker_task(name: &str) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(|ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok(())
    });
    TaskSpec::new(
        name,
        task,
        RestartPolicy::Never,
        BackoffPolicy::default(),
        None,
    )
}

fn instant_task(name: &str) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(|_ctx: TaskContext| async { Ok(()) });
    TaskSpec::new(
        name,
        task,
        RestartPolicy::Never,
        BackoffPolicy::default(),
        None,
    )
}

async fn warm_runtime(handle: &SupervisorHandle, label: &str) {
    let (_, waiter) = handle
        .add_and_watch(instant_task(label))
        .await
        .expect("dynamic benchmark warmup admission failed");
    let outcome = waiter
        .wait()
        .await
        .expect("dynamic benchmark warmup outcome closed");
    assert!(
        matches!(outcome, TaskOutcome::Completed),
        "expected completed warmup, got {outcome:?}"
    );
}

fn bench_add(c: &mut Criterion) {
    print_suite_header("dynamic management");
    let mut group = c.benchmark_group(REGISTRY_ADD.group_id);
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(10));
    group.throughput(Throughput::Elements(1));

    for &(rt_name, rt_fn) in &RUNTIMES {
        group.bench_function(rt_name, |b| {
            record_case(REGISTRY_ADD, rt_name, None);
            b.iter_custom(|iters| {
                let rt = rt_fn();
                rt.block_on(async {
                    let sup = Supervisor::new(bench_config(), vec![]);
                    let handle = sup.serve().expect("runtime startup");
                    warm_runtime(&handle, "warm-add").await;

                    let mut total = Duration::ZERO;
                    for i in 0..iters {
                        let spec = instant_task(&format!("a-{i}"));
                        let start = std::time::Instant::now();
                        let id = handle.add(spec).await.expect("add admission failed");
                        let elapsed = start.elapsed();
                        black_box(id);
                        total += elapsed;
                    }

                    handle.shutdown().await.expect("shutdown failed");
                    total
                })
            });
        });
    }
    group.finish();
}

fn bench_add_cancel(c: &mut Criterion) {
    let mut group = c.benchmark_group(ADD_CANCEL.group_id);
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(10));
    group.throughput(Throughput::Elements(1));

    for &(rt_name, rt_fn) in &RUNTIMES {
        group.bench_function(rt_name, |b| {
            record_case(ADD_CANCEL, rt_name, None);
            b.iter_custom(|iters| {
                let rt = rt_fn();
                rt.block_on(async {
                    let sup = Supervisor::new(bench_config(), vec![]);
                    let handle = sup.serve().expect("runtime startup");
                    warm_runtime(&handle, "warm-add-cancel").await;

                    let mut total = Duration::ZERO;
                    for i in 0..iters {
                        let name = format!("ac-{i}");
                        let spec = worker_task(&name);

                        let start = std::time::Instant::now();
                        let id = handle.add(spec).await.expect("add admission failed");
                        let claimed = handle.cancel(id).await.expect("cancel failed");
                        let elapsed = start.elapsed();
                        assert!(claimed, "benchmark must claim the task it just added");
                        total += elapsed;
                    }

                    handle.shutdown().await.expect("shutdown failed");
                    total
                })
            });
        });
    }
    group.finish();
}

fn bench_list(c: &mut Criterion) {
    let mut group = c.benchmark_group(LIST.group_id);
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(10));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for n_tasks in [10, 100, 500] {
            group.throughput(Throughput::Elements(1));
            group.bench_with_input(
                BenchmarkId::new(rt_name, format!("{n_tasks}_tasks")),
                &n_tasks,
                |b, &n| {
                    record_case(LIST, rt_name, Some(format!("{n}_tasks")));
                    b.iter_custom(|iters| {
                        let rt = rt_fn();
                        rt.block_on(async {
                            let sup = Supervisor::new(bench_config(), vec![]);
                            let handle = sup.serve().expect("runtime startup");

                            for i in 0..n {
                                handle
                                    .add(worker_task(&format!("w-{i}")))
                                    .await
                                    .expect("worker admission failed");
                            }

                            let mut total = Duration::ZERO;
                            for _ in 0..iters {
                                let start = std::time::Instant::now();
                                let snapshot = handle.list().await;
                                let elapsed = start.elapsed();
                                assert_eq!(snapshot.len(), n, "registry snapshot is incomplete");
                                black_box(snapshot);
                                total += elapsed;
                            }

                            handle.shutdown().await.expect("shutdown failed");
                            total
                        })
                    });
                },
            );
        }
    }
    group.finish();
}

fn bench_churn(c: &mut Criterion) {
    let mut group = c.benchmark_group(ADD_SHUTDOWN.group_id);
    group.sample_size(15);
    group.measurement_time(Duration::from_secs(15));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for n_tasks in [10, 100, 500] {
            group.throughput(Throughput::Elements(n_tasks as u64));
            group.bench_with_input(
                BenchmarkId::new(rt_name, format!("{n_tasks}_cleaned_tasks")),
                &n_tasks,
                |b, &n| {
                    record_case(ADD_SHUTDOWN, rt_name, Some(format!("{n}_cleaned_tasks")));
                    b.iter_custom(|iters| {
                        let mut total = Duration::ZERO;
                        for _ in 0..iters {
                            let rt = rt_fn();
                            total += rt.block_on(async {
                                let sup = Supervisor::new(bench_config(), vec![]);
                                let handle = sup.serve().expect("runtime startup");
                                let tasks: Vec<_> =
                                    (0..n).map(|i| worker_task(&format!("ch-{i}"))).collect();

                                let start = std::time::Instant::now();
                                for task in tasks {
                                    handle.add(task).await.expect("churn admission failed");
                                }
                                handle.shutdown().await.expect("shutdown failed");
                                start.elapsed()
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
    bench_add,
    bench_add_cancel,
    bench_list,
    bench_churn
);

fn main() {
    support::benchmark_main("dynamic management", benches);
}
