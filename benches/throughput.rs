//! Performance benchmarks for taskvisor.
//!
//! Measures end-to-end throughput and latency through the full supervisor pipeline.
//!
//! ## Benchmarks
//!
//! | Name | What it measures |
//! |------|-----------------|
//! | `event_throughput` | Events/sec through Bus → SubscriberSet → on_event |
//! | `task_lifecycle` | Single task: add → spawn → run → complete → cleanup |
//! | `subscriber_fanout` | Fan-out overhead: 1 vs 4 vs 8 subscribers |
//! | `concurrent_scaling` | Wall-clock time for N simultaneous tasks |
//!
//! Each benchmark runs on both `current_thread` and `multi_thread` runtimes.
//!
//! ## Run
//! ```bash
//! task cargo/bench
//! # or directly:
//! cargo bench --bench throughput
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

use taskvisor::{
    BackoffPolicy, Event, RestartPolicy, Subscribe, Supervisor,
    SupervisorConfig, TaskFn, TaskRef, TaskSpec,
};

// ---------------------------------------------------------------------------
// Runtime constructors
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Test subscribers
// ---------------------------------------------------------------------------

/// Counts every event delivered. Large queue to avoid overflow noise.
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

/// No-op subscriber — measures pure fan-out overhead.
struct NoopSubscriber {
    id: &'static str,
}

impl NoopSubscriber {
    fn new(id: &'static str) -> Arc<Self> {
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

// ---------------------------------------------------------------------------
// Task helpers
// ---------------------------------------------------------------------------

/// One-shot task that completes immediately. Minimal overhead.
fn instant_task(name: &str) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(name, |_ctx: CancellationToken| async { Ok(()) });
    TaskSpec::new(task, RestartPolicy::Never, BackoffPolicy::default(), None)
}

/// One-shot task with light CPU work to simulate real load.
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

/// Supervisor config tuned for benchmarks: large bus, short grace.
fn bench_config() -> SupervisorConfig {
    let mut cfg = SupervisorConfig::default();
    cfg.bus_capacity = 16384;
    cfg.grace = Duration::from_secs(5);
    cfg
}

// ---------------------------------------------------------------------------
// Benchmark: run N tasks through supervisor, return wall-clock time.
// ---------------------------------------------------------------------------

async fn run_tasks(tasks: Vec<TaskSpec>, subs: Vec<Arc<dyn Subscribe>>) -> Duration {
    let sup = Supervisor::new(bench_config(), subs);
    let start = Instant::now();
    sup.run(tasks).await.unwrap();
    start.elapsed()
}

// ---------------------------------------------------------------------------
// 1. Event throughput
// ---------------------------------------------------------------------------
//
// How many events/sec flow through the full pipeline.
// Each instant task produces ~5-6 events (TaskAddRequested, TaskAdded,
// TaskStarting, TaskStopped, ActorExhausted, TaskRemoved).
// CountingSubscriber tallies them at the end.

fn bench_event_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("event_throughput");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for n_tasks in [50, 200, 500] {
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
                                run_tasks(tasks, subs).await
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

// ---------------------------------------------------------------------------
// 2. Task lifecycle — per-task overhead
// ---------------------------------------------------------------------------
//
// Isolates the cost of one full lifecycle: add → spawn → run → complete → cleanup.
// Single task per iteration, no subscribers — measures pure supervisor overhead.

fn bench_task_lifecycle(c: &mut Criterion) {
    let mut group = c.benchmark_group("task_lifecycle");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(10));

    for &(rt_name, rt_fn) in &RUNTIMES {
        group.bench_function(rt_name, |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for i in 0..iters {
                    let rt = rt_fn();
                    total += rt.block_on(async {
                        let task = instant_task(&format!("lc-{i}"));
                        run_tasks(vec![task], vec![]).await
                    });
                }
                total
            });
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// 3. Subscriber fan-out — overhead vs subscriber count
// ---------------------------------------------------------------------------
//
// Same workload (100 instant tasks), but varying number of subscribers.
// Shows the cost of fan-out: 1 / 4 / 8 subscribers.

fn bench_subscriber_fanout(c: &mut Criterion) {
    let mut group = c.benchmark_group("subscriber_fanout");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));

    // Static names to avoid &'static str lifetime issues.
    const NAMES: [&str; 8] = ["s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7"];
    let n_tasks = 100;

    for &(rt_name, rt_fn) in &RUNTIMES {
        for n_subs in [1usize, 4, 8] {
            group.bench_function(BenchmarkId::new(rt_name, format!("{n_tasks}t_{n_subs}s")), |b| {
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let rt = rt_fn();
                        total += rt.block_on(async {
                            let subs: Vec<Arc<dyn Subscribe>> = (0..n_subs)
                                .map(|i| NoopSubscriber::new(NAMES[i]) as Arc<dyn Subscribe>)
                                .collect();
                            let tasks: Vec<TaskSpec> = (0..n_tasks)
                                .map(|i| instant_task(&format!("fo-{i}")))
                                .collect();
                            run_tasks(tasks, subs).await
                        });
                    }
                    total
                });
            });
        }
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// 4. Concurrent scaling — 10 / 100 / 1000 tasks
// ---------------------------------------------------------------------------
//
// All tasks submitted at once to supervisor.run().
// Tasks do light CPU work (1000 iterations) to simulate real load.
// Measures wall-clock time — shows scheduling and cleanup overhead at scale.

fn bench_concurrent_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_scaling");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(15));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for n_tasks in [10, 100, 1000] {
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
                                run_tasks(tasks, vec![]).await
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

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_event_throughput,
    bench_task_lifecycle,
    bench_subscriber_fanout,
    bench_concurrent_scaling,
);
criterion_main!(benches);
