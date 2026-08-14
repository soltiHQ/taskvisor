//! # Cold batch lifecycle throughput
//!
//! Measures complete static batches on fresh supervisors.
//! The stopwatch includes supervisor construction, first ownership admission, task completion, and shared shutdown cleanup.
//!
//! Run with `cargo bench --bench throughput`.

mod support;

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use taskvisor::TaskContext;
use tokio::runtime::Runtime;

use taskvisor::{
    BackoffPolicy, Event, RestartPolicy, Subscribe, Supervisor, SupervisorConfig, TaskFn, TaskRef,
    TaskSpec,
};

use support::{CaseFamily, print_suite_header, record_case};

const INSTANT_BATCH: CaseFamily = CaseFamily::lifecycle(
    "throughput/cold/full_batch/instant_with_subscriber",
    "COLD BATCH · INSTANT + SUBSCRIBER",
    "completed task",
    "completed tasks",
    "fresh Supervisor construction through batch completion, callback drain, and cleanup",
    "TaskSpec values, subscriber value, and Tokio runtime construction",
);

const CPU_BATCH: CaseFamily = CaseFamily::lifecycle(
    "throughput/cold/full_batch/cpu_no_subscriber",
    "COLD BATCH · CPU WORK",
    "completed task",
    "completed tasks",
    "fresh Supervisor construction through completion and cleanup of tasks with 1,000 CPU-loop iterations each",
    "TaskSpec values and Tokio runtime construction",
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
    fn queue_capacity(&self) -> NonZeroUsize {
        NonZeroUsize::new(16384).unwrap()
    }
}

fn bench_config() -> SupervisorConfig {
    SupervisorConfig::default()
        .with_bus_capacity(NonZeroUsize::new(16384).unwrap())
        .with_grace(Duration::from_secs(5))
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

fn work_task(name: &str, iterations: u64) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(move |_ctx: TaskContext| async move {
        let mut x = 0u64;
        for i in 0..iterations {
            x = std::hint::black_box(x.wrapping_add(i));
        }
        std::hint::black_box(x);
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

fn bench_batch(c: &mut Criterion) {
    print_suite_header("throughput");
    let mut group = c.benchmark_group(INSTANT_BATCH.group_id);
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for n_tasks in [50, 200, 500] {
            group.throughput(Throughput::Elements(n_tasks as u64));
            group.bench_with_input(
                BenchmarkId::new(rt_name, format!("{n_tasks}_completed_tasks")),
                &n_tasks,
                |b, &n| {
                    record_case(INSTANT_BATCH, rt_name, Some(format!("{n}_completed_tasks")));
                    b.iter_custom(|iters| {
                        let mut total = Duration::ZERO;
                        for _ in 0..iters {
                            let rt = rt_fn();
                            total += rt.block_on(async {
                                let counter = CountingSubscriber::new();
                                let subs: Vec<Arc<dyn Subscribe>> = vec![counter.clone()];
                                let tasks: Vec<TaskSpec> =
                                    (0..n).map(|i| instant_task(&format!("t-{i}"))).collect();
                                let start = std::time::Instant::now();
                                let sup = Supervisor::new(bench_config(), subs);
                                sup.run(tasks).await.expect("instant batch failed");
                                let elapsed = start.elapsed();
                                assert!(
                                    counter.count.load(Ordering::Relaxed) > 0,
                                    "subscriber received no lifecycle events"
                                );
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

fn bench_batch_work(c: &mut Criterion) {
    let mut group = c.benchmark_group(CPU_BATCH.group_id);
    group.sample_size(15);
    group.measurement_time(Duration::from_secs(10));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for n_tasks in [10, 100, 500] {
            group.throughput(Throughput::Elements(n_tasks as u64));
            group.bench_with_input(
                BenchmarkId::new(rt_name, format!("{n_tasks}_completed_tasks")),
                &n_tasks,
                |b, &n| {
                    record_case(CPU_BATCH, rt_name, Some(format!("{n}_completed_tasks")));
                    b.iter_custom(|iters| {
                        let mut total = Duration::ZERO;
                        for _ in 0..iters {
                            let rt = rt_fn();
                            total += rt.block_on(async {
                                let tasks: Vec<TaskSpec> =
                                    (0..n).map(|i| work_task(&format!("w-{i}"), 1000)).collect();
                                let start = std::time::Instant::now();
                                let sup = Supervisor::new(bench_config(), vec![]);
                                sup.run(tasks).await.expect("CPU batch failed");
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

fn main() {
    support::benchmark_main("throughput", benches);
}
