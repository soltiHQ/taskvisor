//! # Dynamic: `serve()` API latency
//!
//! Measures the latency of individual `SupervisorHandle` operations against a running supervisor started via `serve()`.
//!
//! ## What is measured
//!
//! | Benchmark       | Description                                              |
//! |-----------------|----------------------------------------------------------|
//! | `add`           | `handle.add(spec)` — time to submit one task.            |
//! | `add_remove`    | `handle.add(spec)` + `handle.cancel(name)` round-trip.   |
//! | `list`          | `handle.list()` with N tasks already running.            |
//! | `churn/N`       | Add N tasks, then shut down — measures cleanup at scale. |
//!
//! ## Run
//!
//! ```bash
//! cargo bench --bench dynamic
//! ```

use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

use taskvisor::{
    BackoffPolicy, RestartPolicy, Supervisor, SupervisorConfig, TaskFn, TaskRef, TaskSpec,
};

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
    let mut cfg = SupervisorConfig::default();
    cfg.bus_capacity = 16384;
    cfg.grace = Duration::from_secs(5);
    cfg
}

fn worker_task(name: &str) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(name, |ctx: CancellationToken| async move {
        ctx.cancelled().await;
        Ok(())
    });
    TaskSpec::new(task, RestartPolicy::Never, BackoffPolicy::default(), None)
}

fn instant_task(name: &str) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(name, |_ctx: CancellationToken| async { Ok(()) });
    TaskSpec::new(task, RestartPolicy::Never, BackoffPolicy::default(), None)
}

fn bench_add(c: &mut Criterion) {
    let mut group = c.benchmark_group("dynamic/add");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(10));
    group.throughput(Throughput::Elements(1));

    for &(rt_name, rt_fn) in &RUNTIMES {
        group.bench_function(rt_name, |b| {
            b.iter_custom(|iters| {
                let rt = rt_fn();
                rt.block_on(async {
                    let sup = Supervisor::new(bench_config(), vec![]);
                    let handle = sup.serve();

                    let mut total = Duration::ZERO;
                    for i in 0..iters {
                        let spec = instant_task(&format!("a-{i}"));
                        let start = std::time::Instant::now();
                        handle.add(spec).unwrap();
                        total += start.elapsed();
                    }

                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let _ = handle.shutdown().await;
                    total
                })
            });
        });
    }
    group.finish();
}

fn bench_add_cancel(c: &mut Criterion) {
    let mut group = c.benchmark_group("dynamic/add_cancel");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(10));
    group.throughput(Throughput::Elements(1));

    for &(rt_name, rt_fn) in &RUNTIMES {
        group.bench_function(rt_name, |b| {
            b.iter_custom(|iters| {
                let rt = rt_fn();
                rt.block_on(async {
                    let sup = Supervisor::new(bench_config(), vec![]);
                    let handle = sup.serve();

                    let wait = Duration::from_secs(5);
                    let mut total = Duration::ZERO;
                    for i in 0..iters {
                        let name = format!("ac-{i}");
                        let spec = worker_task(&name);

                        let start = std::time::Instant::now();
                        handle.add_and_wait(spec, wait).await.unwrap();
                        let _ = handle.cancel(&name).await;
                        total += start.elapsed();
                    }

                    let _ = handle.shutdown().await;
                    total
                })
            });
        });
    }
    group.finish();
}

fn bench_list(c: &mut Criterion) {
    let mut group = c.benchmark_group("dynamic/list");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(10));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for n_tasks in [10, 100, 500] {
            group.throughput(Throughput::Elements(1));
            group.bench_with_input(
                BenchmarkId::new(rt_name, format!("{n_tasks}_tasks")),
                &n_tasks,
                |b, &n| {
                    b.iter_custom(|iters| {
                        let rt = rt_fn();
                        rt.block_on(async {
                            let sup = Supervisor::new(bench_config(), vec![]);
                            let handle = sup.serve();

                            let wait = Duration::from_secs(5);
                            for i in 0..n {
                                handle
                                    .add_and_wait(worker_task(&format!("w-{i}")), wait)
                                    .await
                                    .unwrap();
                            }

                            let mut total = Duration::ZERO;
                            for _ in 0..iters {
                                let start = std::time::Instant::now();
                                let _ = handle.list().await;
                                total += start.elapsed();
                            }

                            let _ = handle.shutdown().await;
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
    let mut group = c.benchmark_group("dynamic/churn");
    group.sample_size(15);
    group.measurement_time(Duration::from_secs(15));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for n_tasks in [10, 100, 500] {
            group.throughput(Throughput::Elements(n_tasks as u64));
            group.bench_with_input(BenchmarkId::new(rt_name, n_tasks), &n_tasks, |b, &n| {
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let rt = rt_fn();
                        total += rt.block_on(async {
                            let sup = Supervisor::new(bench_config(), vec![]);
                            let handle = sup.serve();

                            let wait = Duration::from_secs(5);
                            let start = std::time::Instant::now();
                            for i in 0..n {
                                handle
                                    .add_and_wait(worker_task(&format!("ch-{i}")), wait)
                                    .await
                                    .unwrap();
                            }
                            let _ = handle.shutdown().await;
                            start.elapsed()
                        });
                    }
                    total
                });
            });
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
criterion_main!(benches);
