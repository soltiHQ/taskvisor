//! # Cold single-task lifecycle benchmarks
//!
//! Measures one fresh supervisor from construction through one final task outcome and shared cleanup.
//! Task construction and Tokio runtime construction stay outside the stopwatch.
//!
//! Run with `cargo bench --bench lifecycle`.

mod support;

use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use taskvisor::TaskContext;
use tokio::runtime::Runtime;

use taskvisor::{
    BackoffPolicy, RestartPolicy, Supervisor, SupervisorConfig, TaskFn, TaskRef, TaskSpec,
};

use support::{CaseFamily, print_suite_header, record_case};

const INSTANT: CaseFamily = CaseFamily::lifecycle(
    "lifecycle/cold/full_run/instant",
    "COLD SINGLE TASK · INSTANT",
    "completed task",
    "completed tasks",
    "fresh Supervisor construction through one final task outcome and shared cleanup",
    "TaskSpec and Tokio runtime construction",
);

const CPU_WORK: CaseFamily = CaseFamily::lifecycle(
    "lifecycle/cold/full_run/cpu_work",
    "COLD SINGLE TASK · CPU WORK",
    "completed task",
    "completed tasks",
    "fresh Supervisor construction through one CPU task outcome and shared cleanup",
    "TaskSpec and Tokio runtime construction",
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
        .with_bus_capacity(std::num::NonZeroUsize::new(16384).unwrap())
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

fn bench_instant(c: &mut Criterion) {
    print_suite_header("lifecycle");
    let mut group = c.benchmark_group(INSTANT.group_id);
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(10));
    group.throughput(Throughput::Elements(1));

    for &(rt_name, rt_fn) in &RUNTIMES {
        group.bench_function(rt_name, |b| {
            record_case(INSTANT, rt_name, None);
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for i in 0..iters {
                    let rt = rt_fn();
                    total += rt.block_on(async {
                        let task = instant_task(&format!("lc-{i}"));
                        let start = std::time::Instant::now();
                        let sup = Supervisor::new(bench_config(), vec![]);
                        sup.run(vec![task]).await.expect("cold lifecycle failed");
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
    let mut group = c.benchmark_group(CPU_WORK.group_id);
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(10));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for n in [100, 1_000, 10_000] {
            group.throughput(Throughput::Elements(1));
            group.bench_with_input(
                BenchmarkId::new(rt_name, format!("{n}_iterations")),
                &n,
                |b, &iterations| {
                    record_case(CPU_WORK, rt_name, Some(format!("{iterations}_iterations")));
                    b.iter_custom(|iters| {
                        let mut total = Duration::ZERO;
                        for i in 0..iters {
                            let rt = rt_fn();
                            total += rt.block_on(async {
                                let task = work_task(&format!("w-{i}"), iterations);
                                let start = std::time::Instant::now();
                                let sup = Supervisor::new(bench_config(), vec![]);
                                sup.run(vec![task]).await.expect("CPU lifecycle failed");
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

fn main() {
    support::benchmark_main("lifecycle", benches);
}
