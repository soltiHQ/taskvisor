//! # Fan-out: subscriber overhead
//!
//! Measures how adding subscribers affects total throughput.
//!
//! ## What is measured
//!
//! | Benchmark       | Description                                          |
//! |-----------------|------------------------------------------------------|
//! | `subs/N`        | 100 instant tasks with N no-op subscribers attached. |
//!
//! Subscriber counts: 0, 1, 4, 8.
//!
//! ## Run
//!
//! ```bash
//! cargo bench --bench fanout
//! ```

use std::sync::Arc;
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

struct NoopSubscriber {
    id: &'static str,
}

impl NoopSubscriber {
    fn arc(id: &'static str) -> Arc<Self> {
        Arc::new(Self { id })
    }
}

impl Subscribe for NoopSubscriber {
    fn on_event(&self, _ev: &Event) {}
    fn name(&self) -> &'static str {
        self.id
    }
    fn queue_capacity(&self) -> usize {
        16384
    }
}

const N_TASKS: usize = 100;
const SUB_NAMES: [&str; 8] = ["s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7"];

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

fn bench_fanout(c: &mut Criterion) {
    let mut group = c.benchmark_group("fanout/subs");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));
    group.throughput(Throughput::Elements(N_TASKS as u64));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for n_subs in [0usize, 1, 4, 8] {
            group.bench_function(
                BenchmarkId::new(rt_name, format!("{N_TASKS}t_{n_subs}s")),
                |b| {
                    b.iter_custom(|iters| {
                        let mut total = Duration::ZERO;
                        for _ in 0..iters {
                            let rt = rt_fn();
                            total += rt.block_on(async {
                                let subs: Vec<Arc<dyn Subscribe>> = (0..n_subs)
                                    .map(|i| NoopSubscriber::arc(SUB_NAMES[i]) as Arc<dyn Subscribe>)
                                    .collect();
                                let tasks: Vec<TaskSpec> = (0..N_TASKS)
                                    .map(|i| instant_task(&format!("fo-{i}")))
                                    .collect();
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

criterion_group!(benches, bench_fanout);
criterion_main!(benches);
