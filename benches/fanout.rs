//! # Cold subscriber fan-out benchmarks
//!
//! Measures complete batches of 100 instant tasks on fresh supervisors with 0, 1, 4, or 8 minimal counting subscribers.
//! Supervisor construction is inside the stopwatch for every count.
//!
//! Run with `cargo bench --bench fanout`.

mod support;

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use taskvisor::TaskContext;
use tokio::runtime::Runtime;

use taskvisor::{
    BackoffPolicy, Event, RestartPolicy, Subscribe, Supervisor, SupervisorConfig, TaskFn, TaskRef,
    TaskSpec,
};

use support::{CaseFamily, print_suite_header, record_case};

const FANOUT: CaseFamily = CaseFamily::lifecycle(
    "fanout/cold/full_batch/instant",
    "COLD BATCH · SUBSCRIBER FAN-OUT",
    "completed task",
    "completed tasks",
    "fresh Supervisor through 100 task completions, counting callbacks, drain, and cleanup",
    "TaskSpec values, subscriber values, and Tokio runtime construction",
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
    id: &'static str,
    seen: AtomicUsize,
}

impl CountingSubscriber {
    fn arc(id: &'static str) -> Arc<Self> {
        Arc::new(Self {
            id,
            seen: AtomicUsize::new(0),
        })
    }
}

impl Subscribe for CountingSubscriber {
    fn on_event(&self, _ev: &Event) {
        self.seen.fetch_add(1, Ordering::Relaxed);
    }
    fn name(&self) -> &'static str {
        self.id
    }
    fn queue_capacity(&self) -> NonZeroUsize {
        NonZeroUsize::new(16384).unwrap()
    }
}

const N_TASKS: usize = 100;
const SUB_NAMES: [&str; 8] = ["s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7"];

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

fn bench_fanout(c: &mut Criterion) {
    print_suite_header("fanout");
    let mut group = c.benchmark_group(FANOUT.group_id);
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));
    group.throughput(Throughput::Elements(N_TASKS as u64));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for n_subs in [0usize, 1, 4, 8] {
            group.bench_function(
                BenchmarkId::new(
                    rt_name,
                    format!("{N_TASKS}_completed_tasks_{n_subs}_subscribers"),
                ),
                |b| {
                    record_case(
                        FANOUT,
                        rt_name,
                        Some(format!("{N_TASKS}_completed_tasks_{n_subs}_subscribers")),
                    );
                    b.iter_custom(|iters| {
                        let mut total = Duration::ZERO;
                        for _ in 0..iters {
                            let rt = rt_fn();
                            total += rt.block_on(async {
                                let subscribers: Vec<_> = (0..n_subs)
                                    .map(|i| CountingSubscriber::arc(SUB_NAMES[i]))
                                    .collect();
                                let subs: Vec<Arc<dyn Subscribe>> = subscribers
                                    .iter()
                                    .cloned()
                                    .map(|subscriber| subscriber as Arc<dyn Subscribe>)
                                    .collect();
                                let tasks: Vec<TaskSpec> = (0..N_TASKS)
                                    .map(|i| instant_task(&format!("fo-{i}")))
                                    .collect();
                                let start = std::time::Instant::now();
                                let sup = Supervisor::new(bench_config(), subs);
                                sup.run(tasks).await.expect("fan-out batch failed");
                                let elapsed = start.elapsed();
                                for subscriber in subscribers {
                                    assert!(
                                        subscriber.seen.load(Ordering::Relaxed) > 0,
                                        "subscriber {} received no lifecycle events",
                                        subscriber.id
                                    );
                                }
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

criterion_group!(benches, bench_fanout);

fn main() {
    support::benchmark_main("fanout", benches);
}
