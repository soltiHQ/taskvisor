//! # Lifecycle: per-task overhead
//!
//! Measures the cost of one full task lifecycle through the supervisor pipeline:
//! ```text
//! add → spawn actor → run closure → complete → cleanup → TaskRemoved
//! ```
//!
//! ## What is measured
//!
//! | Benchmark          | Description                                                  |
//! |--------------------|--------------------------------------------------------------|
//! | `instant`          | Task completes immediately (`Ok(())`). Pure framework cost.  |
//! | `with_work/N`      | Task does N iterations of light CPU work before completing.  |
//!
//! Each benchmark runs on both `current_thread` and `multi_thread` runtimes to show scheduling overhead differences.
//!
//! ## Run
//!
//! ```bash
//! cargo bench --bench lifecycle
//! ```

use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

use taskvisor::{BackoffPolicy, RestartPolicy, Supervisor, SupervisorConfig, TaskFn, TaskRef, TaskSpec};

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

fn work_task(name: &str, iterations: u64) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(name, move |_ctx: CancellationToken| async move {
        let mut x = 0u64;
        for i in 0..iterations {
            x = x.wrapping_add(i);
        }
        std::hint::black_box(x);
        Ok(())
    });
    TaskSpec::new(task, RestartPolicy::Never, BackoffPolicy::default(), None)
}

fn bench_instant(c: &mut Criterion) {
    let mut group = c.benchmark_group("lifecycle/instant");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(10));
    group.throughput(Throughput::Elements(1));

    for &(rt_name, rt_fn) in &RUNTIMES {
        group.bench_function(rt_name, |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for i in 0..iters {
                    let rt = rt_fn();
                    total += rt.block_on(async {
                        let task = instant_task(&format!("lc-{i}"));
                        let sup = Supervisor::new(bench_config(), vec![]);
                        let start = std::time::Instant::now();
                        sup.run(vec![task]).await.unwrap();
                        start.elapsed()
                    });
                }
                total
            });
        });
    }
    group.finish();
}

fn bench_with_work(c: &mut Criterion) {
    let mut group = c.benchmark_group("lifecycle/with_work");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(10));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for n in [100, 1_000, 10_000] {
            group.throughput(Throughput::Elements(1));
            group.bench_with_input(
                BenchmarkId::new(rt_name, format!("{n}_iters")),
                &n,
                |b, &iterations| {
                    b.iter_custom(|iters| {
                        let mut total = Duration::ZERO;
                        for i in 0..iters {
                            let rt = rt_fn();
                            total += rt.block_on(async {
                                let task = work_task(&format!("w-{i}"), iterations);
                                let sup = Supervisor::new(bench_config(), vec![]);
                                let start = std::time::Instant::now();
                                sup.run(vec![task]).await.unwrap();
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

criterion_group!(benches, bench_instant, bench_with_work);
criterion_main!(benches);
