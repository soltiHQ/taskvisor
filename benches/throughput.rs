//! # Throughput: tasks per second at scale
//!
//! Measures how many tasks the supervisor can process per second when given a batch of N tasks at once via `sup.run(specs)`.
//!
//! ## What is measured
//!
//! | Benchmark         | Description                                              |
//! |-------------------|----------------------------------------------------------|
//! | `batch/N`         | Run N instant tasks, report tasks/sec.                   |
//! | `batch_work/N`    | Run N tasks with light CPU work (1000 iters), tasks/sec. |
//!
//! ## Run
//!
//! ```bash
//! cargo bench --bench throughput
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

use taskvisor::{
    BackoffPolicy, Event, RestartPolicy, Subscribe, Supervisor, SupervisorConfig, TaskFn, TaskRef,
    TaskSpec,
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

struct CountingSubscriber {
    count: AtomicU64,
}

impl CountingSubscriber {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            count: AtomicU64::new(0),
        })
    }

}

impl Subscribe for CountingSubscriber {
    fn on_event(&self, _ev: &Event) {
        self.count.fetch_add(1, Ordering::Relaxed);
    }
    fn name(&self) -> &'static str {
        "counter"
    }
    fn queue_capacity(&self) -> usize {
        16384
    }
}

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

fn bench_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput/batch");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for n_tasks in [50, 200, 500] {
            group.throughput(Throughput::Elements(n_tasks as u64));
            group.bench_with_input(
                BenchmarkId::new(rt_name, n_tasks),
                &n_tasks,
                |b, &n| {
                    b.iter_custom(|iters| {
                        let mut total = Duration::ZERO;
                        for _ in 0..iters {
                            let rt = rt_fn();
                            total += rt.block_on(async {
                                let counter = CountingSubscriber::new();
                                let subs: Vec<Arc<dyn Subscribe>> = vec![counter.clone()];
                                let tasks: Vec<TaskSpec> =
                                    (0..n).map(|i| instant_task(&format!("t-{i}"))).collect();
                                let sup = Supervisor::new(bench_config(), subs);
                                let start = std::time::Instant::now();
                                sup.run(tasks).await.unwrap();
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

fn bench_batch_work(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput/batch_work");
    group.sample_size(15);
    group.measurement_time(Duration::from_secs(10));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for n_tasks in [10, 100, 500] {
            group.throughput(Throughput::Elements(n_tasks as u64));
            group.bench_with_input(
                BenchmarkId::new(rt_name, n_tasks),
                &n_tasks,
                |b, &n| {
                    b.iter_custom(|iters| {
                        let mut total = Duration::ZERO;
                        for _ in 0..iters {
                            let rt = rt_fn();
                            total += rt.block_on(async {
                                let tasks: Vec<TaskSpec> = (0..n)
                                    .map(|i| work_task(&format!("w-{i}"), 1000))
                                    .collect();
                                let sup = Supervisor::new(bench_config(), vec![]);
                                let start = std::time::Instant::now();
                                sup.run(tasks).await.unwrap();
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

criterion_group!(benches, bench_batch, bench_batch_work);
criterion_main!(benches);
