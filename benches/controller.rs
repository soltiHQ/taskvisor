//! # Controller: admission policy overhead
//!
//! Measures controller submission and admission policies (Queue, Replace, DropIfRunning).
//! The groups cover queued completion, replacement, rejection, the `try_submit` hot path, and multi-slot fan-out.
//!
//! ## What is measured
//!
//! | Benchmark              | Description                                                         |
//! |------------------------|---------------------------------------------------------------------|
//! | `queue/N`              | Submit N tasks (Queue) in one slot; drain to `N` `TaskRemoved`.     |
//! | `replace/N`            | Time N Replace admission decisions (completions vary by design).    |
//! | `drop_if_running/N`    | Time N DropIfRunning admission decisions.                           |
//! | `submit_hotpath/N`     | Isolated synchronous admission cost (`try_submit` enqueue only).    |
//! | `multi_slot/N`         | N submissions fanned out across `SLOTS` distinct slots, drained.    |
//!
//! `queue` and `multi_slot` measure real completion via an event-driven drain (wait for the expected `TaskRemoved` count).
//! `replace`/`drop_if_running` admit a timing-dependent number of runs; time only the admission decisions (teardown is excluded).
//! `submit_hotpath` isolates the caller-side enqueue cost, and `multi_slot` exercises the per-slot `DashMap` sharding the single-slot benches never touch.
//!
//! ## Run
//!
//! ```bash
//! cargo bench --bench controller --features controller
//! ```

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use taskvisor::TaskContext;
use tokio::runtime::Runtime;
use tokio::sync::Notify;

use taskvisor::{
    BackoffPolicy, ControllerConfig, ControllerSpec, Event, EventKind, RestartPolicy, Subscribe,
    Supervisor, SupervisorConfig, TaskFn, TaskRef, TaskSpec,
};

struct RemovalCounter {
    seen: AtomicUsize,
    target: usize,
    done: Notify,
}

impl RemovalCounter {
    fn new(target: usize) -> Arc<Self> {
        Arc::new(Self {
            seen: AtomicUsize::new(0),
            target,
            done: Notify::new(),
        })
    }

    async fn wait_drained(self: &Arc<Self>) {
        loop {
            let notified = self.done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.seen.load(Ordering::Acquire) >= self.target {
                return;
            }
            notified.await;
        }
    }
}

impl Subscribe for RemovalCounter {
    fn on_event(&self, ev: &Event) {
        if ev.kind == EventKind::TaskRemoved
            && self.seen.fetch_add(1, Ordering::AcqRel) + 1 >= self.target
        {
            self.done.notify_waiters();
        }
    }

    fn name(&self) -> &'static str {
        "removal-counter"
    }

    fn queue_capacity(&self) -> usize {
        65536
    }
}

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
    let task: TaskRef = TaskFn::arc(name, |_ctx: TaskContext| async { Ok(()) });
    TaskSpec::new(task, RestartPolicy::Never, BackoffPolicy::default(), None)
}

fn short_task(name: &str) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(name, |ctx: TaskContext| async move {
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
                            let counter = RemovalCounter::new(count);
                            let subs: Vec<Arc<dyn Subscribe>> =
                                vec![Arc::clone(&counter) as Arc<dyn Subscribe>];
                            let sup = Supervisor::builder(bench_config())
                                .with_subscribers(subs)
                                .with_controller(ControllerConfig::default())
                                .build();
                            let handle = sup.serve();

                            let start = std::time::Instant::now();
                            for i in 0..count {
                                let spec = instant_task(&format!("q-{i}"));
                                handle
                                    .submit(ControllerSpec::queue(spec).with_slot("slot-q"))
                                    .await
                                    .unwrap();
                            }
                            let _ = tokio::time::timeout(
                                Duration::from_secs(10),
                                counter.wait_drained(),
                            )
                            .await;
                            let elapsed = start.elapsed();

                            let _ = handle.shutdown().await;
                            elapsed
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
                            for i in 0..count {
                                let spec = short_task(&format!("r-{i}"));
                                handle
                                    .submit(ControllerSpec::replace(spec).with_slot("slot-r"))
                                    .await
                                    .unwrap();
                            }
                            let elapsed = start.elapsed();

                            let _ = handle.shutdown().await;
                            elapsed
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
                            for i in 0..count {
                                let spec = short_task(&format!("d-{i}"));
                                handle
                                    .submit(
                                        ControllerSpec::drop_if_running(spec).with_slot("slot-d"),
                                    )
                                    .await
                                    .unwrap();
                            }
                            let elapsed = start.elapsed();

                            let _ = handle.shutdown().await;
                            elapsed
                        });
                    }
                    total
                });
            });
        }
    }
    group.finish();
}

fn bench_submit_hotpath(c: &mut Criterion) {
    let mut group = c.benchmark_group("controller/submit_hotpath");
    group.sample_size(20);

    for &(rt_name, rt_fn) in &RUNTIMES {
        for n in [100, 500, 1000] {
            group.throughput(Throughput::Elements(n as u64));
            group.bench_with_input(BenchmarkId::new(rt_name, n), &n, |b, &count| {
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let rt = rt_fn();
                        total += rt.block_on(async {
                            let sup = Supervisor::builder(bench_config())
                                .with_controller(
                                    ControllerConfig::default().with_queue_capacity(
                                        NonZeroUsize::new(count.max(1024))
                                            .expect("benchmark queue capacity is non-zero"),
                                    ),
                                )
                                .build();
                            let handle = sup.serve();
                            let template = instant_task("slot-h");

                            let start = std::time::Instant::now();
                            for _ in 0..count {
                                let _ = handle
                                    .try_submit(ControllerSpec::drop_if_running(template.clone()));
                            }
                            let elapsed = start.elapsed();

                            let _ = handle.shutdown().await;
                            elapsed
                        });
                    }
                    total
                });
            });
        }
    }
    group.finish();
}

fn bench_multi_slot(c: &mut Criterion) {
    const SLOTS: usize = 8;

    let mut group = c.benchmark_group("controller/multi_slot");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(15));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for n in [8, 32, 64] {
            group.throughput(Throughput::Elements(n as u64));
            group.bench_with_input(BenchmarkId::new(rt_name, n), &n, |b, &count| {
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let rt = rt_fn();
                        total += rt.block_on(async {
                            let counter = RemovalCounter::new(count);
                            let subs: Vec<Arc<dyn Subscribe>> =
                                vec![Arc::clone(&counter) as Arc<dyn Subscribe>];
                            let sup = Supervisor::builder(bench_config())
                                .with_subscribers(subs)
                                .with_controller(ControllerConfig::default())
                                .build();
                            let handle = sup.serve();

                            let start = std::time::Instant::now();
                            for i in 0..count {
                                let spec = instant_task(&format!("ms-{i}"));
                                handle
                                    .submit(
                                        ControllerSpec::queue(spec)
                                            .with_slot(format!("slot-{}", i % SLOTS)),
                                    )
                                    .await
                                    .unwrap();
                            }
                            let _ = tokio::time::timeout(
                                Duration::from_secs(10),
                                counter.wait_drained(),
                            )
                            .await;
                            let elapsed = start.elapsed();

                            let _ = handle.shutdown().await;
                            elapsed
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
    bench_queue,
    bench_replace,
    bench_drop_if_running,
    bench_submit_hotpath,
    bench_multi_slot,
);
criterion_main!(benches);
