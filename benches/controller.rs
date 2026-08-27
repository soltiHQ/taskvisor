//! # Controller admission and lifecycle benchmarks
//!
//! Separates first-use startup, caller-side intake, verified policy decisions, and controller-managed task outcomes.
//! Steady cases reuse a warmed runtime and supervisor; retained ownership drains outside each measured batch.
//! Each result is checked at its advertised boundary. Intake acceptance does not imply successful task completion.
//!
//! Run with `cargo bench --bench controller --features controller`.

mod support;

use std::hint::black_box;
use std::num::NonZeroUsize;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use taskvisor::{
    BackoffPolicy, ControllerConfig, ControllerSpec, RejectionKind, RestartPolicy, SlotStatusKind,
    Supervisor, SupervisorHandle, TaskContext, TaskFn, TaskId, TaskOutcome, TaskRef, TaskSpec,
    TaskWaiter,
};

use support::fixtures::{
    self, AsyncFlag, RUNTIMES, RtFactory, WATCHDOG, bench_config, expect_canceled,
    expect_completed, expect_within, instant_task, wait_for_ownership,
};
use support::{CaseFamily, print_suite_header, record_case};

const COLD_INTAKE: CaseFamily = CaseFamily::intake(
    "controller/cold/first_try_submit",
    "COLD FIRST TRY_SUBMIT",
    "accepted submission",
    "accepted submissions",
    "first caller-side try_submit on a fresh served supervisor, including lazy cleanup-worker startup",
    "Supervisor/controller startup, request construction, controller decision, task outcome, ownership drain, shutdown, and Tokio runtime construction",
);

const STEADY_INTAKE: CaseFamily = CaseFamily::intake(
    "controller/reused/intake_try_submit",
    "STEADY TRY_SUBMIT BURST",
    "accepted submission",
    "accepted submissions",
    "64 caller-side try_submit acceptances on a reused supervisor; the multi-thread controller can consume concurrently",
    "Supervisor/controller startup, named-slot warmup, request construction, waiting for controller decisions and outcomes, post-batch ownership drain, shutdown, and Tokio runtime construction",
);

const CONCURRENT_INTAKE: CaseFamily = CaseFamily::intake(
    "controller/reused/parked_controller_concurrent_native_try_submit",
    "CONCURRENT NATIVE TRY_SUBMIT · PARKED CONTROLLER",
    "accepted submission",
    "accepted submissions",
    "start-condvar release through completion-condvar observation for 1, 2, 4, or 8 already-spawned native producer threads making exactly 1024 caller-side try_submit calls while the current-thread runtime is synchronously parked and cannot process controller commands",
    "Supervisor/controller startup, named-slot warmup, producer thread spawn/join, request construction and transfer to workers, start-line readiness wait, acceptance checks, all controller processing and outcomes, post-batch ownership/slot drain, shutdown, and Tokio runtime construction",
);

const DROP_REJECTION: CaseFamily = CaseFamily::policy(
    "controller/reused/drop_busy_rejection",
    "DROP_IF_RUNNING · BUSY SLOT",
    "verified rejection",
    "verified rejections",
    "32 watched submissions through verified SlotBusy outcomes against one held owner on a reused supervisor",
    "Supervisor/controller startup, named-slot warmup, held-owner setup/release/cleanup, request construction, post-batch ownership drain, shutdown, and Tokio runtime construction",
);

const REPLACE_PLACEMENT: CaseFamily = CaseFamily::policy(
    "controller/reused/replace_busy_placement",
    "REPLACE · BUSY SLOT",
    "processed replacement",
    "processed replacements",
    "32 watched Replace submissions through 31 SupersededByReplace outcomes after the newest request has replaced the queue head",
    "Supervisor/controller startup, named-slot warmup, held-owner setup/release, request construction, retention snapshot check, newest task completion, post-batch ownership drain, shutdown, and Tokio runtime construction",
);

const QUEUE_ONE: CaseFamily = CaseFamily::lifecycle(
    "controller/reused/queue_one_slot",
    "QUEUE · ONE SLOT",
    "completed task",
    "completed tasks",
    "32 watched submissions through Completed outcomes in one slot on a reused current-thread runtime and supervisor",
    "Supervisor/controller startup, named-slot warmup, request construction, post-outcome ownership drain and slot reset, shutdown, and Tokio runtime construction",
);

const QUEUE_EIGHT: CaseFamily = CaseFamily::lifecycle(
    "controller/reused/queue_eight_slots",
    "QUEUE · EIGHT SLOTS",
    "completed task",
    "completed tasks",
    "64 watched submissions through Completed outcomes across eight named slots on a reused runtime and supervisor after warming those admission paths",
    "Supervisor/controller startup, named-slot warmup, request construction, post-outcome ownership drain and slot reset, shutdown, and Tokio runtime construction",
);

fn held_owner_task(
    name: impl Into<Arc<str>>,
    started: Arc<AsyncFlag>,
    release: Arc<AsyncFlag>,
) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(move |ctx: TaskContext| {
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        async move {
            started.mark();
            ctx.cancelled().await;
            release.wait().await;
            Ok(())
        }
    });
    TaskSpec::new(
        name,
        task,
        RestartPolicy::Never,
        BackoffPolicy::default(),
        None,
    )
}

async fn expect_rejected(waiter: TaskWaiter, expected: RejectionKind) {
    let outcome = expect_within("a controller rejection", waiter.wait())
        .await
        .expect("submission outcome channel closed");
    assert!(
        matches!(outcome, TaskOutcome::Rejected { kind, .. } if kind == expected),
        "expected {expected:?}, got {outcome:?}"
    );
}

async fn drain_controller(handle: &SupervisorHandle) {
    // An outcome does not imply retained-value destruction or controller slot GC.
    // With no new submissions, both observations must settle before the next batch.
    wait_for_ownership(handle, 0).await;
    expect_within("controller slots to become idle", async {
        loop {
            let snapshot = handle
                .controller_snapshot()
                .await
                .expect("benchmark controller must exist");
            if snapshot.slots.iter().all(|slot| {
                slot.status == SlotStatusKind::Idle
                    && slot.owner_id.is_none()
                    && slot.queue_depth == 0
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
}

async fn warm_controller_slots(handle: &SupervisorHandle, slots: &[&str]) {
    for slot in slots {
        let (_, waiter) = handle
            .submit_and_watch(
                ControllerSpec::queue(instant_task(format!("warm-{slot}"))).with_slot(*slot),
            )
            .await
            .expect("controller warmup intake failed");
        expect_completed(waiter).await;
    }
    // Idle slots may be collected. Warm the same admission paths and wait for
    // their previous owners to leave instead of preserving a stale slot owner.
    drain_controller(handle).await;
}

enum NativeProducerCommand {
    Run(Vec<ControllerSpec>),
    Stop,
}

#[derive(Default)]
struct NativeProducerState {
    ready: usize,
    released: bool,
    completed: usize,
    accepted: usize,
    first_error: Option<String>,
}

struct NativeProducerGate {
    expected: usize,
    state: Mutex<NativeProducerState>,
    changed: Condvar,
}

impl NativeProducerGate {
    fn new(expected: usize) -> Arc<Self> {
        assert!(expected != 0, "a producer gate needs at least one caller");
        Arc::new(Self {
            expected,
            state: Mutex::new(NativeProducerState::default()),
            changed: Condvar::new(),
        })
    }

    fn reset(&self) {
        let mut state = self.state.lock().expect("producer gate lock poisoned");
        assert!(
            (state.ready == 0 && state.completed == 0)
                || (state.ready == self.expected && state.completed == self.expected),
            "producer gate reset before the previous batch completed"
        );
        *state = NativeProducerState::default();
    }

    fn arrive_and_wait(&self) {
        let mut state = self.state.lock().expect("producer gate lock poisoned");
        state.ready += 1;
        assert!(
            state.ready <= self.expected,
            "more producers reached the gate than configured"
        );
        self.changed.notify_all();
        while !state.released {
            state = self
                .changed
                .wait(state)
                .expect("producer gate lock poisoned while parked");
        }
    }

    fn wait_until_ready(&self) {
        let state = self.state.lock().expect("producer gate lock poisoned");
        let (state, timeout) = self
            .changed
            .wait_timeout_while(state, WATCHDOG, |state| state.ready != self.expected)
            .expect("producer gate lock poisoned while waiting for readiness");
        assert!(
            !timeout.timed_out() && state.ready == self.expected,
            "benchmark timed out while parking all native producers"
        );
    }

    fn release(&self) {
        let mut state = self.state.lock().expect("producer gate lock poisoned");
        assert_eq!(
            state.ready, self.expected,
            "producer batch released before every caller was ready"
        );
        state.released = true;
        self.changed.notify_all();
    }

    fn complete(&self, accepted: usize, first_error: Option<String>) {
        let mut state = self.state.lock().expect("producer gate lock poisoned");
        state.accepted += accepted;
        if state.first_error.is_none() {
            state.first_error = first_error;
        }
        state.completed += 1;
        assert!(
            state.completed <= self.expected,
            "more producers completed than configured"
        );
        self.changed.notify_all();
    }

    fn wait_until_complete(&self) -> (usize, Option<String>) {
        let state = self.state.lock().expect("producer gate lock poisoned");
        let (state, timeout) = self
            .changed
            .wait_timeout_while(state, WATCHDOG, |state| state.completed != self.expected)
            .expect("producer gate lock poisoned while waiting for completion");
        assert!(
            !timeout.timed_out() && state.completed == self.expected,
            "benchmark timed out while waiting for native producers"
        );
        (state.accepted, state.first_error.clone())
    }
}

struct NativeProducerPool {
    commands: Vec<mpsc::SyncSender<NativeProducerCommand>>,
    joins: Vec<thread::JoinHandle<()>>,
    gate: Arc<NativeProducerGate>,
}

impl NativeProducerPool {
    fn new(handle: &SupervisorHandle, producers: usize) -> Self {
        let gate = NativeProducerGate::new(producers);
        let mut commands = Vec::with_capacity(producers);
        let mut joins = Vec::with_capacity(producers);

        for producer in 0..producers {
            let (command_tx, command_rx) = mpsc::sync_channel(0);
            let producer_handle = handle.clone();
            let producer_gate = Arc::clone(&gate);
            let join = thread::Builder::new()
                .name(format!("taskvisor-bench-producer-{producer}"))
                .spawn(move || {
                    while let Ok(command) = command_rx.recv() {
                        let NativeProducerCommand::Run(requests) = command else {
                            break;
                        };
                        producer_gate.arrive_and_wait();

                        let mut accepted = 0usize;
                        let mut first_error = None;
                        for request in requests {
                            match producer_handle.try_submit(request) {
                                Ok(id) => {
                                    accepted += 1;
                                    black_box(id);
                                }
                                Err(error) => {
                                    if first_error.is_none() {
                                        first_error = Some(format!("{error:?}"));
                                    }
                                }
                            }
                        }
                        producer_gate.complete(accepted, first_error);
                    }
                })
                .expect("native benchmark producer thread startup");
            commands.push(command_tx);
            joins.push(join);
        }

        Self {
            commands,
            joins,
            gate,
        }
    }

    fn prepare(&self, batches: Vec<Vec<ControllerSpec>>) {
        assert_eq!(batches.len(), self.commands.len());
        self.gate.reset();
        for (command, requests) in self.commands.iter().zip(batches) {
            command
                .send(NativeProducerCommand::Run(requests))
                .expect("native benchmark producer stopped before its batch");
        }
        self.gate.wait_until_ready();
    }

    fn release(&self) {
        self.gate.release();
    }

    fn wait_until_complete(&self) -> (usize, Option<String>) {
        self.gate.wait_until_complete()
    }

    fn shutdown(mut self) {
        for command in &self.commands {
            command
                .send(NativeProducerCommand::Stop)
                .expect("native benchmark producer stopped before shutdown");
        }
        for join in self.joins.drain(..) {
            join.join()
                .expect("native benchmark producer thread panicked");
        }
    }
}

async fn start_held_owner(
    handle: &SupervisorHandle,
    slot: &str,
    name: &str,
) -> (TaskId, TaskWaiter, Arc<AsyncFlag>) {
    let started = AsyncFlag::new();
    let release = AsyncFlag::new();
    let (_, waiter) = handle
        .submit_and_watch(
            ControllerSpec::queue(held_owner_task(
                name,
                Arc::clone(&started),
                Arc::clone(&release),
            ))
            .with_slot(slot),
        )
        .await
        .expect("held owner intake failed");
    let id = waiter.id();
    expect_within("the held owner to start", started.wait()).await;
    expect_within("the held owner to reach the Running slot state", async {
        loop {
            let snapshot = handle
                .controller_snapshot()
                .await
                .expect("benchmark controller must exist");
            if snapshot.slot(slot).is_some_and(|view| {
                view.owner_id == Some(id)
                    && view.status == SlotStatusKind::Running
                    && view.queue_depth == 0
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    wait_for_ownership(handle, 1).await;
    (id, waiter, release)
}

fn bench_cold_first_try_submit(c: &mut Criterion) {
    print_suite_header("controller");
    let mut group = c.benchmark_group(COLD_INTAKE.group_id);
    group.throughput(Throughput::Elements(1));

    for &(rt_name, rt_fn) in &RUNTIMES {
        group.bench_function(rt_name, |b| {
            record_case(COLD_INTAKE, rt_name, None);
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for i in 0..iters {
                    let rt = rt_fn();
                    total += rt.block_on(async {
                        let supervisor = Supervisor::builder(bench_config())
                            .with_controller(ControllerConfig::default())
                            .build();
                        let handle = supervisor.serve().expect("runtime startup");
                        let request = ControllerSpec::queue(instant_task(format!("cold-{i}")))
                            .with_slot("cold-slot");

                        let start = Instant::now();
                        let id = handle
                            .try_submit(request)
                            .expect("first controller intake failed");
                        let elapsed = start.elapsed();
                        black_box(id);

                        drain_controller(&handle).await;
                        handle.shutdown().await.expect("shutdown failed");
                        elapsed
                    });
                }
                total
            });
        });
    }
    group.finish();
}

fn bench_steady_try_submit(c: &mut Criterion) {
    const COUNT: usize = 64;
    const VALUE: &str = "64_accepted_submissions";

    let mut group = c.benchmark_group(STEADY_INTAKE.group_id);
    group.throughput(Throughput::Elements(COUNT as u64));

    for &(rt_name, rt_fn) in &RUNTIMES {
        group.bench_function(BenchmarkId::new(rt_name, VALUE), |b| {
            record_case(STEADY_INTAKE, rt_name, Some(VALUE.to_owned()));
            b.iter_custom(|iters| {
                let rt = rt_fn();
                rt.block_on(async {
                    let supervisor = Supervisor::builder(bench_config())
                        .with_controller(
                            ControllerConfig::default()
                                .with_queue_capacity(NonZeroUsize::new(COUNT).unwrap()),
                        )
                        .build();
                    let handle = supervisor.serve().expect("runtime startup");
                    warm_controller_slots(&handle, &["intake-slot"]).await;

                    let mut total = Duration::ZERO;
                    for iteration in 0..iters {
                        let mut requests: Vec<_> = (0..COUNT)
                            .map(|i| {
                                ControllerSpec::drop_if_running(instant_task(format!(
                                    "intake-{iteration}-{i}"
                                )))
                                .with_slot("intake-slot")
                            })
                            .collect();

                        let start = Instant::now();
                        for request in requests.drain(..) {
                            let id = handle
                                .try_submit(request)
                                .expect("steady try_submit intake failed");
                            black_box(id);
                        }
                        total += start.elapsed();

                        // These units are accepted submissions, not successful tasks.
                        // Either policy rejection or completion must release ownership.
                        drain_controller(&handle).await;
                    }
                    handle.shutdown().await.expect("shutdown failed");
                    total
                })
            });
        });
    }
    group.finish();
}

fn bench_concurrent_try_submit(c: &mut Criterion) {
    const COUNT: usize = 1_024;

    let mut group = c.benchmark_group(CONCURRENT_INTAKE.group_id);
    group.throughput(Throughput::Elements(COUNT as u64));

    for producers in [1usize, 2, 4, 8] {
        let rt_name = "current_thread";
        let producer_label = if producers == 1 {
            "native_producer"
        } else {
            "native_producers"
        };
        let parameter = format!("{COUNT}_accepted_submissions_{producers}_{producer_label}");
        group.bench_function(BenchmarkId::new(rt_name, &parameter), |b| {
            record_case(CONCURRENT_INTAKE, rt_name, Some(parameter.clone()));
            b.iter_custom(|iters| {
                let rt = fixtures::rt_current_thread();
                rt.block_on(async {
                    let batch_capacity = NonZeroUsize::new(COUNT).unwrap();
                    let supervisor = Supervisor::builder(
                        bench_config().with_ownership_capacity(Some(batch_capacity)),
                    )
                    .with_controller(
                        ControllerConfig::default().with_queue_capacity(batch_capacity),
                    )
                    .build();
                    let handle = supervisor.serve().expect("runtime startup");
                    warm_controller_slots(&handle, &["concurrent-intake-slot"]).await;
                    let producer_pool = NativeProducerPool::new(&handle, producers);

                    let mut total = Duration::ZERO;
                    for iteration in 0..iters {
                        let per_producer = COUNT / producers;
                        assert_eq!(per_producer * producers, COUNT);
                        let batches: Vec<Vec<_>> = (0..producers)
                            .map(|producer| {
                                (0..per_producer)
                                    .map(|i| {
                                        ControllerSpec::drop_if_running(instant_task(format!(
                                            "concurrent-intake-{iteration}-{producer}-{i}"
                                        )))
                                        .with_slot("concurrent-intake-slot")
                                    })
                                    .collect()
                            })
                            .collect();
                        producer_pool.prepare(batches);

                        // This is the `Runtime::block_on` root on a current-thread runtime.
                        // Its synchronous completion wait parks the only runtime thread, so
                        // the controller cannot process these commands inside the timer.
                        let start = Instant::now();
                        producer_pool.release();
                        let (accepted, first_error) = producer_pool.wait_until_complete();
                        let elapsed = start.elapsed();

                        assert_eq!(accepted, COUNT);
                        assert!(
                            first_error.is_none(),
                            "concurrent try_submit intake failed: {first_error:?}"
                        );
                        total += elapsed;

                        drain_controller(&handle).await;
                    }

                    producer_pool.shutdown();
                    handle.shutdown().await.expect("shutdown failed");
                    total
                })
            });
        });
    }
    group.finish();
}

fn bench_drop_busy_rejection(c: &mut Criterion) {
    const COUNT: usize = 32;
    const VALUE: &str = "32_verified_rejections";

    let mut group = c.benchmark_group(DROP_REJECTION.group_id);
    group.throughput(Throughput::Elements(COUNT as u64));

    for &(rt_name, rt_fn) in &RUNTIMES {
        group.bench_function(BenchmarkId::new(rt_name, VALUE), |b| {
            record_case(DROP_REJECTION, rt_name, Some(VALUE.to_owned()));
            b.iter_custom(|iters| {
                let rt = rt_fn();
                rt.block_on(async {
                    let supervisor = Supervisor::builder(bench_config())
                        .with_controller(ControllerConfig::default())
                        .build();
                    let handle = supervisor.serve().expect("runtime startup");
                    warm_controller_slots(&handle, &["drop-slot"]).await;
                    let (owner_id, owner_waiter, release) =
                        start_held_owner(&handle, "drop-slot", "drop-owner").await;

                    let mut total = Duration::ZERO;
                    for iteration in 0..iters {
                        let mut requests: Vec<_> = (0..COUNT)
                            .map(|i| {
                                ControllerSpec::drop_if_running(instant_task(format!(
                                    "drop-{iteration}-{i}"
                                )))
                                .with_slot("drop-slot")
                            })
                            .collect();
                        let mut waiters = Vec::with_capacity(COUNT);

                        let start = Instant::now();
                        for request in requests.drain(..) {
                            let (_, waiter) = handle
                                .submit_and_watch(request)
                                .await
                                .expect("DropIfRunning intake failed");
                            waiters.push(waiter);
                        }
                        for waiter in waiters.drain(..) {
                            expect_rejected(waiter, RejectionKind::SlotBusy).await;
                        }
                        total += start.elapsed();

                        wait_for_ownership(&handle, 1).await;
                    }

                    release.mark();
                    assert!(
                        handle.cancel(owner_id).await.expect("owner cancel failed"),
                        "benchmark must claim the held owner"
                    );
                    expect_canceled(owner_waiter).await;
                    drain_controller(&handle).await;
                    handle.shutdown().await.expect("shutdown failed");
                    total
                })
            });
        });
    }
    group.finish();
}

fn bench_replace_busy_placement(c: &mut Criterion) {
    const COUNT: usize = 32;
    const VALUE: &str = "32_processed_replacements";

    let mut group = c.benchmark_group(REPLACE_PLACEMENT.group_id);
    group.throughput(Throughput::Elements(COUNT as u64));

    for &(rt_name, rt_fn) in &RUNTIMES {
        group.bench_function(BenchmarkId::new(rt_name, VALUE), |b| {
            record_case(REPLACE_PLACEMENT, rt_name, Some(VALUE.to_owned()));
            b.iter_custom(|iters| {
                let rt = rt_fn();
                rt.block_on(async {
                    let supervisor = Supervisor::builder(bench_config())
                        .with_controller(ControllerConfig::default())
                        .build();
                    let handle = supervisor.serve().expect("runtime startup");
                    warm_controller_slots(&handle, &["replace-slot"]).await;

                    let mut total = Duration::ZERO;
                    for iteration in 0..iters {
                        let (owner_id, owner_waiter, release) = start_held_owner(
                            &handle,
                            "replace-slot",
                            &format!("replace-owner-{iteration}"),
                        )
                        .await;
                        let mut requests: Vec<_> = (0..COUNT)
                            .map(|i| {
                                ControllerSpec::replace(instant_task(format!(
                                    "replace-{iteration}-{i}"
                                )))
                                .with_slot("replace-slot")
                            })
                            .collect();
                        let mut waiters = Vec::with_capacity(COUNT);

                        let start = Instant::now();
                        for request in requests.drain(..) {
                            let (_, waiter) = handle
                                .submit_and_watch(request)
                                .await
                                .expect("Replace intake failed");
                            waiters.push(waiter);
                        }
                        let newest = waiters.pop().expect("replacement batch is non-empty");
                        for waiter in waiters.drain(..) {
                            expect_rejected(waiter, RejectionKind::SupersededByReplace).await;
                        }
                        total += start.elapsed();

                        // Head replacement happens before the displaced waiter is resolved.
                        // All 31 rejections therefore prove that the newest head was installed.
                        // Check the held-owner/retained-head state without timing a query.
                        let snapshot = handle
                            .controller_snapshot()
                            .await
                            .expect("benchmark controller must exist");
                        let slot = snapshot
                            .slot("replace-slot")
                            .expect("the held owner must retain its slot");
                        assert_eq!(slot.owner_id, Some(owner_id));
                        assert_eq!(slot.status, SlotStatusKind::Terminating);
                        assert_eq!(slot.queue_depth, 1);

                        release.mark();
                        expect_canceled(owner_waiter).await;
                        expect_completed(newest).await;
                        drain_controller(&handle).await;
                    }
                    handle.shutdown().await.expect("shutdown failed");
                    total
                })
            });
        });
    }
    group.finish();
}

fn bench_queue_workload(
    c: &mut Criterion,
    family: CaseFamily,
    count: usize,
    slots: &[&str],
    runtimes: &[(&str, RtFactory)],
) {
    let value = format!("{count}_completed_tasks");
    let mut group = c.benchmark_group(family.group_id);
    group.throughput(Throughput::Elements(count as u64));

    for &(rt_name, rt_fn) in runtimes {
        group.bench_function(BenchmarkId::new(rt_name, &value), |b| {
            record_case(family, rt_name, Some(value.clone()));
            b.iter_custom(|iters| {
                let rt = rt_fn();
                rt.block_on(async {
                    let supervisor = Supervisor::builder(bench_config())
                        .with_controller(ControllerConfig::default())
                        .build();
                    let handle = supervisor.serve().expect("runtime startup");
                    warm_controller_slots(&handle, slots).await;

                    let mut total = Duration::ZERO;
                    for iteration in 0..iters {
                        let mut requests: Vec<_> = (0..count)
                            .map(|i| {
                                ControllerSpec::queue(instant_task(format!(
                                    "queue-{iteration}-{i}"
                                )))
                                .with_slot(slots[i % slots.len()])
                            })
                            .collect();
                        let mut waiters = Vec::with_capacity(count);

                        let start = Instant::now();
                        for request in requests.drain(..) {
                            let (_, waiter) = handle
                                .submit_and_watch(request)
                                .await
                                .expect("Queue intake failed");
                            waiters.push(waiter);
                        }
                        for waiter in waiters.drain(..) {
                            expect_completed(waiter).await;
                        }
                        total += start.elapsed();

                        drain_controller(&handle).await;
                    }
                    handle.shutdown().await.expect("shutdown failed");
                    total
                })
            });
        });
    }
    group.finish();
}

fn bench_queue_one_slot(c: &mut Criterion) {
    bench_queue_workload(
        c,
        QUEUE_ONE,
        32,
        &["queue-slot"],
        &[("current_thread", fixtures::rt_current_thread)],
    );
}

fn bench_queue_eight_slots(c: &mut Criterion) {
    bench_queue_workload(
        c,
        QUEUE_EIGHT,
        64,
        &[
            "multi-slot-0",
            "multi-slot-1",
            "multi-slot-2",
            "multi-slot-3",
            "multi-slot-4",
            "multi-slot-5",
            "multi-slot-6",
            "multi-slot-7",
        ],
        &RUNTIMES,
    );
}

criterion_group! {
    name = benches;
    config = fixtures::criterion();
    targets =
        bench_cold_first_try_submit,
        bench_steady_try_submit,
        bench_concurrent_try_submit,
        bench_drop_busy_rejection,
        bench_replace_busy_placement,
        bench_queue_one_slot,
        bench_queue_eight_slots,
}

fn main() {
    support::benchmark_main("controller", benches);
}
