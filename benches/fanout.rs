//! # Subscriber delivery and slow-observer isolation
//!
//! Fast cases await every completed-task event at every subscriber on a prewarmed supervisor.
//! The overload case holds one callback behind a gate with a one-event queue while tasks and a
//! healthy subscriber make progress. Gate holding and overflow-drain time stay outside its timer.
//!
//! Run with cargo bench --bench fanout.

mod support;

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use taskvisor::{Event, EventKind, Subscribe, Supervisor};

use support::fixtures::{
    BlockingGate, EventCounter, RUNTIMES, ReleaseOnDrop, bench_config, complete_batch,
    expect_completed, expect_within, instant_task, wait_for_ownership, warm_runtime,
};
use support::{CaseFamily, print_suite_header, record_case};

const TASKS: usize = 256;
const SUBSCRIBER_NAMES: [&str; 8] = ["s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7"];

const DELIVERY: CaseFamily = CaseFamily::lifecycle(
    "fanout/steady/verified_delivery",
    "COMPLETION + SUBSCRIBER DELIVERY",
    "completed task",
    "completed tasks",
    "first watched add through 256 Completed outcomes and 256 completed-task events at each counting subscriber",
    "startup, warmup, TaskSpec construction, ownership reset between batches, and shutdown",
);

const SATURATED: CaseFamily = CaseFamily::lifecycle(
    "fanout/steady/isolated_saturated_subscriber",
    "COMPLETION WITH A SATURATED SUBSCRIBER",
    "completed task",
    "completed tasks",
    "256 watched task completions and their delivery to a healthy subscriber while a second callback is blocked with queue capacity 1",
    "startup, warmup, gate setup, TaskSpec construction, gate release and overflow verification, ownership reset, and shutdown",
);

fn bench_delivery(c: &mut Criterion) {
    print_suite_header("fanout");
    let mut group = c.benchmark_group(DELIVERY.group_id);
    group.throughput(Throughput::Elements(TASKS as u64));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for subscriber_count in [1usize, 4, 8] {
            let parameter = format!("{TASKS}_tasks_{subscriber_count}_subscribers");
            group.bench_function(BenchmarkId::new(rt_name, &parameter), |b| {
                record_case(DELIVERY, rt_name, Some(parameter.clone()));
                let rt = rt_fn();
                b.iter_custom(|iters| {
                    rt.block_on(async {
                        let counters: Vec<_> = SUBSCRIBER_NAMES[..subscriber_count]
                            .iter()
                            .map(|&name| EventCounter::new(name, EventKind::TaskFinished))
                            .collect();
                        let subscribers = counters
                            .iter()
                            .map(|counter| Arc::clone(counter) as Arc<dyn Subscribe>)
                            .collect();
                        let handle = Supervisor::new(bench_config(), subscribers)
                            .serve()
                            .expect("runtime startup");
                        warm_runtime(&handle, subscriber_count).await;
                        for counter in &counters {
                            counter.wait_for_count(1).await;
                            counter.assert_healthy();
                        }
                        let mut total = Duration::ZERO;

                        for _ in 0..iters {
                            let expected = counters[0].count() + TASKS;
                            let tasks = (0..TASKS)
                                .map(|i| instant_task(format!("fanout-{i}")))
                                .collect();

                            let start = Instant::now();
                            complete_batch(&handle, tasks).await;
                            for counter in &counters {
                                counter.wait_for_count(expected).await;
                            }
                            total += start.elapsed();

                            for counter in &counters {
                                assert_eq!(counter.count(), expected);
                                counter.assert_healthy();
                            }
                            wait_for_ownership(&handle, subscriber_count).await;
                        }

                        handle.shutdown().await.expect("shutdown failed");
                        for counter in &counters {
                            counter.assert_healthy();
                        }
                        total
                    })
                });
            });
        }
    }
    group.finish();
}

struct HeldSubscriber {
    next_gate: Mutex<Option<Arc<BlockingGate>>>,
    observed: Arc<EventCounter>,
}

impl HeldSubscriber {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            next_gate: Mutex::new(None),
            observed: EventCounter::new("held", EventKind::TaskFinished),
        })
    }

    fn arm(&self, gate: Arc<BlockingGate>) {
        let previous = self
            .next_gate
            .lock()
            .expect("subscriber gate lock")
            .replace(gate);
        assert!(previous.is_none(), "previous gate was never entered");
    }
}

impl Subscribe for HeldSubscriber {
    fn on_event(&self, event: &Event) {
        let gate = self.next_gate.lock().expect("subscriber gate lock").take();
        if let Some(gate) = gate {
            gate.wait();
        }
        self.observed.on_event(event);
    }

    fn name(&self) -> &str {
        "held"
    }

    fn queue_capacity(&self) -> NonZeroUsize {
        NonZeroUsize::new(1).unwrap()
    }
}

fn bench_saturated_subscriber(c: &mut Criterion) {
    let mut group = c.benchmark_group(SATURATED.group_id);
    group.throughput(Throughput::Elements(TASKS as u64));

    for &(rt_name, rt_fn) in &RUNTIMES {
        let parameter = format!("{TASKS}_tasks_1_healthy_1_blocked");
        group.bench_function(BenchmarkId::new(rt_name, &parameter), |b| {
            record_case(SATURATED, rt_name, Some(parameter.clone()));
            let rt = rt_fn();
            b.iter_custom(|iters| {
                rt.block_on(async {
                    let healthy = EventCounter::new("healthy", EventKind::TaskFinished);
                    let held = HeldSubscriber::new();
                    let subscribers: Vec<Arc<dyn Subscribe>> = vec![held.clone(), healthy.clone()];
                    let handle = Supervisor::new(bench_config(), subscribers)
                        .serve()
                        .expect("runtime startup");
                    warm_runtime(&handle, 2).await;
                    healthy.wait_for_count(1).await;
                    let mut total = Duration::ZERO;

                    for _ in 0..iters {
                        let gate = BlockingGate::new();
                        let _release = ReleaseOnDrop::new(Arc::clone(&gate));
                        held.arm(Arc::clone(&gate));
                        let marker_count = healthy.count() + 1;
                        let (_, waiter) = expect_within(
                            "gate marker admission",
                            handle.add_and_watch(instant_task("gate-marker")),
                        )
                        .await
                        .expect("gate marker admission failed");
                        expect_completed(waiter).await;
                        healthy.wait_for_count(marker_count).await;
                        gate.wait_until_blocked().await;
                        wait_for_ownership(&handle, 2).await;
                        let previous_dropped = held.observed.dropped();
                        let expected = healthy.count() + TASKS;
                        let tasks = (0..TASKS)
                            .map(|i| instant_task(format!("saturated-{i}")))
                            .collect();

                        gate.assert_blocked();
                        let start = Instant::now();
                        complete_batch(&handle, tasks).await;
                        healthy.wait_for_count(expected).await;
                        total += start.elapsed();

                        gate.assert_blocked();
                        assert_eq!(healthy.count(), expected);
                        healthy.assert_healthy();
                        gate.release();
                        held.observed
                            .wait_for_overflow_after(previous_dropped)
                            .await;
                        held.observed.assert_no_failures();
                        wait_for_ownership(&handle, 2).await;
                    }

                    handle.shutdown().await.expect("shutdown failed");
                    healthy.assert_healthy();
                    held.observed.assert_no_failures();
                    total
                })
            });
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = support::fixtures::criterion();
    targets = bench_delivery, bench_saturated_subscriber
}

fn main() {
    support::benchmark_main("fanout", benches);
}
