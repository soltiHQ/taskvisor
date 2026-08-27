//! Benchmarks subscriber delivery and slow-observer isolation.

mod support;

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use taskvisor::{Event, EventKind, Subscribe, SubscriberExecution, Supervisor};

use support::fixtures::{
    BlockingGate, EventCounter, RUNTIMES, ReleaseOnDrop, bench_config, complete_batch,
    expect_completed, expect_within, instant_task, wait_for_ownership, warm_runtime,
};
use support::{CaseFamily, print_suite_header, record_case};

const TASKS: usize = 256;
const SUBSCRIBER_NAMES: [&str; 8] = ["s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7"];

const DELIVERY: CaseFamily = CaseFamily::lifecycle(
    "fanout/steady/verified_delivery",
    "COMPLETION + SHARED SUBSCRIBER DELIVERY",
    "completed task",
    "completed tasks",
    "first watched add through 256 Completed outcomes and matching TaskFinished delivery at every configured short-callback subscriber on the default shared worker; zero subscribers is the bus-disabled reference",
    "startup, warmup, TaskSpec construction, ownership reset between batches, and shutdown",
);

const DEDICATED_DELIVERY: CaseFamily = CaseFamily::lifecycle(
    "fanout/steady/verified_dedicated_delivery",
    "COMPLETION + DEDICATED SUBSCRIBER DELIVERY",
    "completed task",
    "completed tasks",
    "first watched add through 256 Completed outcomes and matching TaskFinished delivery at 1, 2, 4, or 8 short-callback subscribers, each with its own dedicated worker",
    "startup, warmup, TaskSpec construction, ownership reset between batches, and shutdown",
);

const SATURATED: CaseFamily = CaseFamily::lifecycle(
    "fanout/steady/isolated_saturated_subscriber",
    "COMPLETION WITH A SATURATED SUBSCRIBER",
    "completed task",
    "completed tasks",
    "256 watched task completions and their delivery to every configured shared healthy subscriber while one dedicated callback is blocked with queue capacity 1",
    "startup, warmup, gate setup, TaskSpec construction, gate release and overflow verification, ownership reset, and shutdown",
);

#[derive(Clone, Copy)]
enum CountingExecution {
    Shared,
    Dedicated,
}

struct DedicatedCounter {
    counter: Arc<EventCounter>,
}

impl Subscribe for DedicatedCounter {
    fn on_event(&self, event: &Event) {
        self.counter.on_event(event);
    }

    fn execution(&self) -> SubscriberExecution {
        SubscriberExecution::Dedicated
    }

    fn name(&self) -> &str {
        self.counter.name()
    }

    fn queue_capacity(&self) -> NonZeroUsize {
        self.counter.queue_capacity()
    }
}

fn counting_subscribers(
    subscriber_count: usize,
    execution: CountingExecution,
) -> (Vec<Arc<EventCounter>>, Vec<Arc<dyn Subscribe>>) {
    let counters: Vec<_> = SUBSCRIBER_NAMES[..subscriber_count]
        .iter()
        .map(|&name| EventCounter::new(name, EventKind::TaskFinished))
        .collect();
    let subscribers = counters
        .iter()
        .map(|counter| match execution {
            CountingExecution::Shared => Arc::clone(counter) as Arc<dyn Subscribe>,
            CountingExecution::Dedicated => Arc::new(DedicatedCounter {
                counter: Arc::clone(counter),
            }) as Arc<dyn Subscribe>,
        })
        .collect();
    (counters, subscribers)
}

fn bench_delivery_family(
    c: &mut Criterion,
    family: CaseFamily,
    subscriber_counts: &[usize],
    execution: CountingExecution,
) {
    let mut group = c.benchmark_group(family.group_id);
    group.throughput(Throughput::Elements(TASKS as u64));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for &subscriber_count in subscriber_counts {
            let parameter = format!("{TASKS}_tasks_{subscriber_count}_subscribers");
            group.bench_function(BenchmarkId::new(rt_name, &parameter), |b| {
                record_case(family, rt_name, Some(parameter.clone()));
                let rt = rt_fn();
                b.iter_custom(|iters| {
                    rt.block_on(async {
                        let (counters, subscribers) =
                            counting_subscribers(subscriber_count, execution);
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
                            let expected: Vec<_> = counters
                                .iter()
                                .map(|counter| counter.count() + TASKS)
                                .collect();
                            let tasks = (0..TASKS)
                                .map(|i| instant_task(format!("fanout-{i}")))
                                .collect();

                            let start = Instant::now();
                            complete_batch(&handle, tasks).await;
                            for (counter, expected) in counters.iter().zip(&expected) {
                                counter.wait_for_count(*expected).await;
                            }
                            total += start.elapsed();

                            for (counter, expected) in counters.iter().zip(&expected) {
                                assert_eq!(counter.count(), *expected);
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

fn bench_delivery(c: &mut Criterion) {
    print_suite_header("fanout");
    bench_delivery_family(c, DELIVERY, &[0, 1, 2, 4, 8], CountingExecution::Shared);
    bench_delivery_family(
        c,
        DEDICATED_DELIVERY,
        &[1, 2, 4, 8],
        CountingExecution::Dedicated,
    );
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

    fn execution(&self) -> SubscriberExecution {
        SubscriberExecution::Dedicated
    }

    fn queue_capacity(&self) -> NonZeroUsize {
        NonZeroUsize::new(1).unwrap()
    }
}

fn bench_saturated_subscriber(c: &mut Criterion) {
    let mut group = c.benchmark_group(SATURATED.group_id);
    group.throughput(Throughput::Elements(TASKS as u64));

    for &(rt_name, rt_fn) in &RUNTIMES {
        for healthy_count in [1usize, 3, 7] {
            let parameter = format!("{TASKS}_tasks_{healthy_count}_healthy_1_blocked");
            group.bench_function(BenchmarkId::new(rt_name, &parameter), |b| {
                record_case(SATURATED, rt_name, Some(parameter.clone()));
                let rt = rt_fn();
                b.iter_custom(|iters| {
                    rt.block_on(async {
                        let healthy: Vec<_> = SUBSCRIBER_NAMES[..healthy_count]
                            .iter()
                            .map(|&name| EventCounter::new(name, EventKind::TaskFinished))
                            .collect();
                        let held = HeldSubscriber::new();
                        let mut subscribers: Vec<Arc<dyn Subscribe>> =
                            Vec::with_capacity(healthy_count + 1);
                        subscribers.push(held.clone());
                        subscribers.extend(
                            healthy
                                .iter()
                                .map(|counter| Arc::clone(counter) as Arc<dyn Subscribe>),
                        );
                        let retained_subscribers = healthy_count + 1;
                        let handle = Supervisor::new(bench_config(), subscribers)
                            .serve()
                            .expect("runtime startup");
                        warm_runtime(&handle, retained_subscribers).await;
                        for counter in &healthy {
                            counter.wait_for_count(1).await;
                        }
                        let mut total = Duration::ZERO;

                        for iteration in 0..iters {
                            let gate = BlockingGate::new();
                            let _release = ReleaseOnDrop::new(Arc::clone(&gate));
                            held.arm(Arc::clone(&gate));
                            let marker_counts: Vec<_> =
                                healthy.iter().map(|counter| counter.count() + 1).collect();
                            let waiter = expect_within(
                                "gate marker admission",
                                handle
                                    .add(instant_task(format!("gate-marker-{iteration}")))
                                    .watch()
                                    .execute(),
                            )
                            .await
                            .expect("gate marker admission failed");
                            expect_completed(waiter).await;
                            for (counter, expected) in healthy.iter().zip(&marker_counts) {
                                counter.wait_for_count(*expected).await;
                            }
                            gate.wait_until_blocked().await;
                            wait_for_ownership(&handle, retained_subscribers).await;
                            let previous_dropped = held.observed.dropped();
                            let expected: Vec<_> = healthy
                                .iter()
                                .map(|counter| counter.count() + TASKS)
                                .collect();
                            let tasks = (0..TASKS)
                                .map(|i| instant_task(format!("saturated-{iteration}-{i}")))
                                .collect();

                            gate.assert_blocked();
                            let start = Instant::now();
                            complete_batch(&handle, tasks).await;
                            for (counter, expected) in healthy.iter().zip(&expected) {
                                counter.wait_for_count(*expected).await;
                            }
                            total += start.elapsed();

                            gate.assert_blocked();
                            for (counter, expected) in healthy.iter().zip(&expected) {
                                assert_eq!(counter.count(), *expected);
                                counter.assert_healthy();
                            }
                            gate.release();
                            held.observed
                                .wait_for_overflow_after(previous_dropped)
                                .await;
                            held.observed.assert_no_failures();
                            wait_for_ownership(&handle, retained_subscribers).await;
                        }

                        handle.shutdown().await.expect("shutdown failed");
                        for counter in &healthy {
                            counter.assert_healthy();
                        }
                        held.observed.assert_no_failures();
                        total
                    })
                });
            });
        }
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
