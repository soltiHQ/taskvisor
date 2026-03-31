//! # Controller: admission policy overhead
//!
//! The controller wraps `handle.add()` with admission policies (Queue, Replace, DropIfRunning).
//! This benchmark compares direct `add()` vs `submit()` to isolate the controller's cost, and then compares the three policies.
//!
//! ## What is measured
//!
//! | Benchmark              | Description                                            |
//! |------------------------|--------------------------------------------------------|
//! | `submit_queue/N`       | Submit N tasks with Queue policy (FIFO sequential).    |
//! | `submit_replace/N`     | Submit N tasks with Replace policy (latest-wins).      |
//! | `submit_drop/N`        | Submit N tasks with DropIfRunning policy.              |
//!
//! ## Run
//!
//! ```bash
//! cargo bench --bench controller --features controller
//! ```

use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

use taskvisor::{
    BackoffPolicy, ControllerConfig, ControllerSpec, RestartPolicy, Supervisor, SupervisorConfig,
    TaskFn, TaskRef, TaskSpec,
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

fn instant_task(name: &str) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(name, |_ctx: CancellationToken| async { Ok(()) });
    TaskSpec::new(task, RestartPolicy::Never, BackoffPolicy::default(), None)
}

fn short_task(name: &str) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(name, |ctx: CancellationToken| async move {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(5)) => Ok(()),
            _ = ctx.cancelled() => Ok(()),
        }
    });
    TaskSpec::new(task, RestartPolicy::Never, BackoffPolicy::default(), None)
}

fn bench_queue(c: &mut Criterion) {
    let mut group = c.benchmark_group("controller/queue");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(15));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for n in [5, 20, 50] {
            group.throughput(Throughput::Elements(n as u64));
            group.bench_with_input(BenchmarkId::new(rt_name, n), &n, |b, &count| {
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let rt = rt_fn();
                        total += rt.block_on(async {
                            let sup = Supervisor::builder(bench_config())
                                .with_controller(ControllerConfig::default())
                                .build();
                            let handle = sup.serve();

                            let start = std::time::Instant::now();
                            for _ in 0..count {
                                let spec = instant_task("slot-q");
                                handle.submit(ControllerSpec::queue(spec)).await.unwrap();
                            }
                            tokio::time::sleep(Duration::from_millis(200)).await;
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

fn bench_replace(c: &mut Criterion) {
    let mut group = c.benchmark_group("controller/replace");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(15));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for n in [5, 20, 50] {
            group.throughput(Throughput::Elements(n as u64));
            group.bench_with_input(BenchmarkId::new(rt_name, n), &n, |b, &count| {
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let rt = rt_fn();
                        total += rt.block_on(async {
                            let sup = Supervisor::builder(bench_config())
                                .with_controller(ControllerConfig::default())
                                .build();
                            let handle = sup.serve();

                            let start = std::time::Instant::now();
                            for _ in 0..count {
                                let spec = short_task("slot-r");
                                handle.submit(ControllerSpec::replace(spec)).await.unwrap();
                            }
                            tokio::time::sleep(Duration::from_millis(200)).await;
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

fn bench_drop_if_running(c: &mut Criterion) {
    let mut group = c.benchmark_group("controller/drop_if_running");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(15));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for n in [5, 20, 50] {
            group.throughput(Throughput::Elements(n as u64));
            group.bench_with_input(BenchmarkId::new(rt_name, n), &n, |b, &count| {
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let rt = rt_fn();
                        total += rt.block_on(async {
                            let sup = Supervisor::builder(bench_config())
                                .with_controller(ControllerConfig::default())
                                .build();
                            let handle = sup.serve();

                            let start = std::time::Instant::now();
                            for _ in 0..count {
                                let spec = short_task("slot-d");
                                handle
                                    .submit(ControllerSpec::drop_if_running(spec))
                                    .await
                                    .unwrap();
                            }
                            tokio::time::sleep(Duration::from_millis(200)).await;
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

criterion_group!(benches, bench_queue, bench_replace, bench_drop_if_running);
criterion_main!(benches);
