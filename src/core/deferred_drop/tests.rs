//! Verifies deferred-drop ownership from reservation through worker cleanup.

use super::*;
use std::{
    future::Future,
    io,
    num::NonZeroUsize,
    pin::Pin,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    task::Poll,
    time::Duration,
};

fn test_executor(worker_count: usize, capacity: usize) -> Arc<DropExecutor> {
    let capacity = NonZeroUsize::new(capacity).expect("test capacity must be non-zero");
    DropExecutor::try_start_with(worker_count, Some(capacity), system_spawner())
        .expect("the test destructor executor must start")
}

fn unlimited_test_executor(worker_count: usize) -> Arc<DropExecutor> {
    DropExecutor::try_start_with(worker_count, None, system_spawner())
        .expect("the unlimited test destructor executor must start")
}

#[derive(Default)]
struct GateState {
    entered: bool,
    released: bool,
}

struct BlockingDrop(Arc<(Mutex<GateState>, Condvar)>);

struct ReleaseGate(Arc<(Mutex<GateState>, Condvar)>);

impl Drop for ReleaseGate {
    fn drop(&mut self) {
        release_gate(&self.0);
    }
}

impl Drop for BlockingDrop {
    fn drop(&mut self) {
        let (state, ready) = &*self.0;
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        state.entered = true;
        ready.notify_all();
        while !state.released {
            state = ready.wait(state).unwrap_or_else(|error| error.into_inner());
        }
    }
}

struct ObservedDrop(Arc<AtomicBool>);

impl Drop for ObservedDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn wait_gate_entered(gate: &Arc<(Mutex<GateState>, Condvar)>) {
    let (state, ready) = &**gate;
    let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
    while !state.entered {
        let (next, timeout) = ready
            .wait_timeout(state, Duration::from_secs(2))
            .unwrap_or_else(|error| error.into_inner());
        state = next;
        assert!(
            !timeout.timed_out() || state.entered,
            "blocking destructor must start"
        );
    }
}

fn release_gate(gate: &Arc<(Mutex<GateState>, Condvar)>) {
    let (state, ready) = &**gate;
    state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .released = true;
    ready.notify_all();
}

fn wait_observed(observed: &AtomicBool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !observed.load(Ordering::Acquire) {
        assert!(
            std::time::Instant::now() < deadline,
            "cleanup must complete before the test deadline"
        );
        std::thread::yield_now();
    }
}

async fn assert_pending_once<F: Future>(mut future: Pin<&mut F>) {
    std::future::poll_fn(|context| match future.as_mut().poll(context) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("the ownership request completed before capacity existed"),
    })
    .await;
}

async fn wait_for_effective_capacity(executor: &DropExecutor, expected: usize) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while executor.capacity.effective_capacity() != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the worker must commit its ownership-capacity update");
}

#[tokio::test(flavor = "current_thread")]
async fn batch_reservation_is_atomic_and_rejects_domain_limit_overflow() {
    let executor = test_executor(1, 2);
    let held = executor.try_reserve().expect("one initial slot");
    let mut batch = Box::pin(executor.reserve_many(2));
    assert_pending_once(batch.as_mut()).await;
    drop(batch);
    assert_eq!(
        executor.capacity.available(),
        1,
        "canceling an unsatisfied atomic batch must return every internally held permit"
    );

    let batch = executor.reserve_many(2);
    drop(held);
    let reservations = batch.await.expect("both slots become available together");
    assert_eq!(reservations.len(), 2);
    drop(reservations);
    assert_eq!(executor.capacity.available(), 2);

    assert!(
        executor.reserve_many(3).await.is_err(),
        "a batch larger than the domain budget must fail without waiting"
    );
}

#[test]
fn try_batch_reservation_is_atomic_and_returns_partial_capacity() {
    let executor = test_executor(1, 2);
    let held = executor.try_reserve().expect("one initial slot");

    assert!(
        executor.try_reserve_many(2).is_err(),
        "a fail-fast batch cannot consume only the one available slot"
    );
    assert_eq!(executor.capacity.available(), 1);

    drop(held);
    let reservations = executor
        .try_reserve_many(2)
        .expect("the full batch fits after both slots are available");
    assert_eq!(reservations.len(), 2);
    drop(reservations);
    assert_eq!(executor.capacity.available(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn unsatisfied_atomic_batch_does_not_block_single_slot_admission() {
    let executor = test_executor(1, 2);
    let held = executor.try_reserve().expect("one initial slot");
    let mut batch = Box::pin(executor.reserve_many(2));
    assert_pending_once(batch.as_mut()).await;

    let single = tokio::time::timeout(Duration::from_secs(1), executor.reserve())
        .await
        .expect("an unsatisfied batch must not head-of-line block a usable slot")
        .expect("the ownership executor remains open");
    assert_pending_once(batch.as_mut()).await;

    drop(single);
    assert_pending_once(batch.as_mut()).await;
    drop(held);
    let reservations = tokio::time::timeout(Duration::from_secs(1), batch)
        .await
        .expect("the atomic batch must wake when its full capacity becomes available")
        .expect("the ownership executor remains open");
    assert_eq!(reservations.len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn waiting_atomic_batch_gets_priority_after_bounded_single_bypass() {
    let executor = test_executor(1, 2);
    let held = executor.try_reserve().expect("one initial slot");
    let mut batch = Box::pin(executor.reserve_many(2));
    assert_pending_once(batch.as_mut()).await;

    for _ in 0..2 {
        let single = tokio::time::timeout(Duration::from_secs(1), executor.reserve())
            .await
            .expect("one capacity turnover may bypass the unsatisfied batch")
            .expect("the ownership executor remains open");
        drop(single);
    }

    let mut next_single = Box::pin(executor.reserve());
    assert_pending_once(next_single.as_mut()).await;
    drop(held);

    let reservations = tokio::time::timeout(Duration::from_secs(1), batch)
        .await
        .expect("bounded bypass must let the older atomic batch accumulate capacity")
        .expect("the ownership executor remains open");
    assert_pending_once(next_single.as_mut()).await;

    drop(reservations);
    tokio::time::timeout(Duration::from_secs(1), next_single)
        .await
        .expect("single-slot admission resumes after the fair batch grant")
        .expect("the ownership executor remains open");
}

#[tokio::test(flavor = "current_thread")]
async fn pending_admission_metadata_is_bounded_by_broker_capacity() {
    let executor = test_executor(1, 1);
    let held = executor.try_reserve().expect("one initial slot");
    let mut first = Box::pin(executor.reserve());
    assert_pending_once(first.as_mut()).await;

    assert!(
        executor.reserve().await.is_err(),
        "a second pending request must be rejected at the waiter budget"
    );
    drop(first);
    drop(held);
    assert_eq!(executor.capacity.available(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn unlimited_admission_is_immediate_and_never_queues_waiters() {
    let executor = unlimited_test_executor(1);
    let reservations = executor
        .reserve_many(4096)
        .await
        .expect("an unlimited broker admits the complete non-zero batch");
    assert_eq!(reservations.len(), 4096);
    assert_eq!(executor.capacity.waiter_count(), 0);

    let additional = executor
        .try_reserve()
        .expect("held unlimited permits do not restrict later admission");
    assert_eq!(executor.capacity.waiter_count(), 0);
    drop(additional);
    drop(reservations);
}

#[test]
fn unlimited_retirement_does_not_reduce_admission() {
    let executor = unlimited_test_executor(1);
    let permit = executor
        .capacity
        .try_acquire(8)
        .expect("an unlimited broker admits the initial batch");
    permit.retire();

    let next = executor
        .capacity
        .try_acquire(8)
        .expect("retirement is accounting-neutral in unlimited mode");
    drop(next);
    assert_eq!(executor.capacity.waiter_count(), 0);
}

#[test]
fn closing_unlimited_admission_returns_an_unbounded_error() {
    let executor = unlimited_test_executor(1);
    executor.capacity.close();

    let error = executor
        .try_reserve()
        .err()
        .expect("a closed unlimited broker rejects new admission");
    assert_eq!(error.limit(), None);
    assert_eq!(executor.capacity.waiter_count(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn canceling_a_notified_grant_returns_its_capacity() {
    let executor = test_executor(1, 1);
    let held = executor.try_reserve().expect("the only slot");
    let mut waiter = Box::pin(executor.reserve());
    assert_pending_once(waiter.as_mut()).await;

    drop(held);
    drop(waiter);

    let recovered = executor
        .try_reserve()
        .expect("canceling a granted but unobserved waiter must return its slot");
    drop(recovered);
    assert_eq!(executor.capacity.available(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn reservation_bounds_running_queued_and_pre_submit_ownership() {
    let executor = test_executor(1, 2);
    let first = Arc::new((Mutex::new(GateState::default()), Condvar::new()));
    let second = Arc::new((Mutex::new(GateState::default()), Condvar::new()));
    executor
        .try_reserve()
        .expect("first slot")
        .bundle(BlockingDrop(Arc::clone(&first)))
        .submit();
    wait_gate_entered(&first);
    executor
        .try_reserve()
        .expect("second slot")
        .bundle(BlockingDrop(Arc::clone(&second)))
        .submit();
    wait_gate_entered(&second);

    for _ in 0..128 {
        assert!(
            executor.try_reserve().is_err(),
            "saturation returns ownership to the pre-transfer caller"
        );
    }

    release_gate(&first);
    release_gate(&second);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if executor.capacity.available() == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("clean bundle completion must return both reservations");
}

#[test]
fn dormant_domain_zero_batch_and_drop_spawn_no_workers() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_spawner = Arc::clone(&calls);
    let spawner: Arc<WorkerSpawner> = Arc::new(move |_worker, _launcher| {
        calls_for_spawner.fetch_add(1, Ordering::AcqRel);
        Err(io::Error::other(
            "a dormant domain must not invoke its spawner",
        ))
    });
    let domain = DropDomain::unstarted_with(CORE_WORKER_COUNT, 4, spawner)
        .expect("the dormant configuration is valid");

    assert_eq!(domain.capacity().map(NonZeroUsize::get), Some(4));
    assert!(!domain.is_started());
    assert!(
        domain
            .try_reserve_many(0)
            .expect("an empty batch needs no executor")
            .is_empty()
    );
    assert!(matches!(
        domain.try_reserve_many(5),
        Err(DropAdmissionError::Capacity(DropCapacityError {
            limit: Some(limit)
        })) if limit.get() == 4
    ));
    assert!(!domain.is_started());
    drop(domain);
    assert_eq!(calls.load(Ordering::Acquire), 0);
}

#[test]
fn dormant_finite_and_unlimited_snapshots_do_not_start_workers() {
    let finite = DropDomain::unstarted(NonZeroUsize::new(4));
    let finite_snapshot = finite.snapshot(true);
    assert_eq!(finite_snapshot.configured_limit, Some(4));
    assert_eq!(finite_snapshot.effective_limit, Some(4));
    assert_eq!(finite_snapshot.available, Some(4));
    assert_eq!(finite_snapshot.retired(), Some(0));
    assert_eq!(finite_snapshot.in_use(), Some(0));
    assert_eq!(finite_snapshot.waiters, 0);
    assert!(finite_snapshot.admission_open);
    assert_eq!(finite_snapshot.cleanup_queued, 0);
    assert_eq!(finite_snapshot.cleanup_running, 0);
    assert!(!finite.is_started());

    let unlimited = DropDomain::unstarted(None);
    let unlimited_snapshot = unlimited.snapshot(false);
    assert_eq!(unlimited_snapshot.configured_limit, None);
    assert_eq!(unlimited_snapshot.effective_limit, None);
    assert_eq!(unlimited_snapshot.available, None);
    assert_eq!(unlimited_snapshot.retired(), None);
    assert_eq!(unlimited_snapshot.in_use(), None);
    assert_eq!(unlimited_snapshot.waiters, 0);
    assert!(!unlimited_snapshot.admission_open);
    assert_eq!(unlimited_snapshot.cleanup_queued, 0);
    assert_eq!(unlimited_snapshot.cleanup_running, 0);
    assert!(!unlimited.is_started());
}

#[tokio::test(flavor = "current_thread")]
async fn broker_snapshot_tracks_parked_and_unobserved_grants() {
    let executor = test_executor(1, 1);
    let held = executor.try_reserve().expect("the initial ownership unit");
    let mut waiter = Box::pin(executor.reserve());
    assert_pending_once(waiter.as_mut()).await;

    let parked = executor.snapshot().capacity;
    assert_eq!(parked.available, Some(0));
    assert_eq!(parked.waiters, 1);
    assert!(parked.open);

    drop(held);
    let granted = executor.snapshot().capacity;
    assert_eq!(granted.available, Some(0));
    assert_eq!(granted.waiters, 0);

    drop(waiter);
    let canceled = executor.snapshot().capacity;
    assert_eq!(canceled.available, Some(1));
    assert_eq!(canceled.waiters, 0);
}

#[test]
fn cleanup_snapshot_tracks_claimed_and_queued_batches() {
    let spawner: Arc<WorkerSpawner> = Arc::new(move |worker, launcher| {
        if worker > 0 {
            return Err(io::Error::other("keep the second cleanup batch queued"));
        }
        std::thread::Builder::new().spawn(move || {
            if let Ok(launch) = launcher.recv() {
                worker_loop(launch);
            }
        })
    });
    let domain = DropDomain::try_start_with(1, 3, spawner)
        .expect("the single core cleanup worker must start");
    let gate = Arc::new((Mutex::new(GateState::default()), Condvar::new()));
    let _release_on_failure = ReleaseGate(Arc::clone(&gate));
    domain
        .try_reserve()
        .expect("blocking cleanup reservation")
        .bundle(BlockingDrop(Arc::clone(&gate)))
        .submit();
    wait_gate_entered(&gate);

    let completed = Arc::new(AtomicBool::new(false));
    domain
        .try_reserve()
        .expect("queued cleanup reservation")
        .bundle(ObservedDrop(Arc::clone(&completed)))
        .submit();

    let blocked = domain.snapshot(true);
    assert_eq!(blocked.in_use(), Some(2));
    assert_eq!(blocked.cleanup_running, 1);
    assert_eq!(blocked.cleanup_queued, 1);

    release_gate(&gate);
    wait_observed(&completed);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let drained = domain.snapshot(true);
        if drained.cleanup_running == 0 && drained.cleanup_queued == 0 {
            assert_eq!(drained.in_use(), Some(0));
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "cleanup accounting must drain after both destructors return"
        );
        std::thread::yield_now();
    }
}

#[test]
fn unlimited_snapshot_keeps_real_cleanup_counts() {
    let domain = DropDomain::unstarted(None);
    let gate = Arc::new((Mutex::new(GateState::default()), Condvar::new()));
    let _release_on_failure = ReleaseGate(Arc::clone(&gate));
    domain
        .try_reserve()
        .expect("unlimited admission must reserve immediately")
        .bundle(BlockingDrop(Arc::clone(&gate)))
        .submit();
    wait_gate_entered(&gate);

    let blocked = domain.snapshot(true);
    assert_eq!(blocked.configured_limit, None);
    assert_eq!(blocked.effective_limit, None);
    assert_eq!(blocked.available, None);
    assert_eq!(blocked.in_use(), None);
    assert_eq!(blocked.waiters, 0);
    assert_eq!(blocked.cleanup_running, 1);

    release_gate(&gate);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while domain.snapshot(true).cleanup_running != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "unlimited cleanup accounting must drain"
        );
        std::thread::yield_now();
    }
}

#[test]
fn concurrent_first_reservations_publish_one_transactional_core() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_spawner = Arc::clone(&calls);
    let spawner: Arc<WorkerSpawner> = Arc::new(move |_worker, launcher| {
        calls_for_spawner.fetch_add(1, Ordering::AcqRel);
        std::thread::Builder::new().spawn(move || {
            if let Ok(launch) = launcher.recv() {
                worker_loop(launch);
            }
        })
    });
    let domain = DropDomain::unstarted_with(CORE_WORKER_COUNT, 8, spawner)
        .expect("the lazy configuration is valid");
    let callers = 8;
    let gate = Arc::new(std::sync::Barrier::new(callers));
    let mut threads = Vec::with_capacity(callers);
    for _ in 0..callers {
        let domain = domain.clone();
        let gate = Arc::clone(&gate);
        threads.push(std::thread::spawn(move || {
            gate.wait();
            domain
                .try_reserve()
                .expect("the shared first startup must admit every caller")
        }));
    }

    let reservations: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().expect("the admission caller must not panic"))
        .collect();
    assert!(domain.is_started());
    assert_eq!(calls.load(Ordering::Acquire), CORE_WORKER_COUNT);
    drop(reservations);
    assert_eq!(domain.started_executor().capacity.available(), callers);
}

#[test]
fn partial_core_failure_leaves_lazy_domain_unstarted_and_capacity_neutral_for_retry() {
    let injected = TestLazyDomain::fail_first_start_at_worker(4, 1);
    let domain = injected.domain();

    let error = match domain.try_reserve() {
        Err(DropAdmissionError::Start(error)) => error,
        Err(DropAdmissionError::Capacity(_)) => {
            panic!("a fresh domain cannot fail capacity admission")
        }
        Ok(_) => panic!("the injected first core startup must fail"),
    };
    assert_eq!(error.worker(), 1);
    assert_eq!(error.source_kind(), io::ErrorKind::Other);
    assert_eq!(error.raw_os_error(), None);
    assert!(!domain.is_started());
    assert_eq!(injected.spawn_calls(), 2);

    let reservation = domain
        .try_reserve()
        .expect("a fresh transactional core must be built on retry");
    assert!(domain.is_started());
    assert_eq!(injected.spawn_calls(), 2 + CORE_WORKER_COUNT);
    assert_eq!(domain.started_executor().capacity.available(), 3);
    drop(reservation);
    assert_eq!(domain.started_executor().capacity.available(), 4);
}

#[test]
fn reservation_keeps_started_executor_alive_after_every_domain_clone_drops() {
    let domain = DropDomain::unstarted(NonZeroUsize::new(1));
    let clone = domain.clone();
    let reservation = domain
        .try_reserve()
        .expect("the first admission starts the executor");
    let executor = domain.started_executor();
    let weak = Arc::downgrade(&executor);
    drop(executor);
    drop(domain);
    drop(clone);

    assert!(
        weak.upgrade().is_some(),
        "a charged reservation must retain its executor independently of domain handles"
    );
    let observed = Arc::new(AtomicBool::new(false));
    reservation
        .bundle(ObservedDrop(Arc::clone(&observed)))
        .submit();
    wait_observed(&observed);

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while weak.upgrade().is_some() {
        assert!(
            std::time::Instant::now() < deadline,
            "the completed final reservation must release the executor"
        );
        std::thread::yield_now();
    }
}

#[test]
fn two_blocked_core_workers_do_not_stop_the_third_cleanup() {
    let domain = DropDomain::try_start(3).expect("the three-worker domain must start");
    let first = Arc::new((Mutex::new(GateState::default()), Condvar::new()));
    let second = Arc::new((Mutex::new(GateState::default()), Condvar::new()));
    let completed = Arc::new(AtomicBool::new(false));

    domain
        .try_reserve()
        .expect("first reservation")
        .bundle(BlockingDrop(Arc::clone(&first)))
        .submit();
    wait_gate_entered(&first);
    domain
        .try_reserve()
        .expect("second reservation")
        .bundle(BlockingDrop(Arc::clone(&second)))
        .submit();
    wait_gate_entered(&second);
    domain
        .try_reserve()
        .expect("third reservation")
        .bundle(ObservedDrop(Arc::clone(&completed)))
        .submit();

    wait_observed(&completed);
    release_gate(&first);
    release_gate(&second);
}

#[test]
fn saturated_domain_does_not_interfere_with_another_domain() {
    let blocked =
        DropDomain::try_start_with(1, 1, system_spawner()).expect("the blocked domain must start");
    let independent = DropDomain::try_start_with(1, 1, system_spawner())
        .expect("the independent domain must start");
    let gate = Arc::new((Mutex::new(GateState::default()), Condvar::new()));
    let completed = Arc::new(AtomicBool::new(false));

    blocked
        .try_reserve()
        .expect("blocked reservation")
        .bundle(BlockingDrop(Arc::clone(&gate)))
        .submit();
    wait_gate_entered(&gate);
    assert!(blocked.try_reserve().is_err());

    independent
        .try_reserve()
        .expect("independent reservation")
        .bundle(ObservedDrop(Arc::clone(&completed)))
        .submit();
    wait_observed(&completed);
    release_gate(&gate);
}

#[test]
fn elastic_worker_advances_cleanup_when_all_core_workers_are_blocked() {
    let domain = DropDomain::try_start(4).expect("the elastic domain must start");
    let gates: Vec<_> = (0..CORE_WORKER_COUNT)
        .map(|_| Arc::new((Mutex::new(GateState::default()), Condvar::new())))
        .collect();
    for gate in &gates {
        domain
            .try_reserve()
            .expect("blocking reservation")
            .bundle(BlockingDrop(Arc::clone(gate)))
            .submit();
        wait_gate_entered(gate);
    }

    let completed = Arc::new(AtomicBool::new(false));
    domain
        .try_reserve()
        .expect("elastic reservation")
        .bundle(ObservedDrop(Arc::clone(&completed)))
        .submit();
    wait_observed(&completed);

    for gate in &gates {
        release_gate(gate);
    }
}

#[test]
fn elastic_activation_cascades_through_already_queued_blockers() {
    let domain = DropDomain::try_start(6).expect("the elastic domain must start");
    let core_gates: Vec<_> = (0..CORE_WORKER_COUNT)
        .map(|_| Arc::new((Mutex::new(GateState::default()), Condvar::new())))
        .collect();
    for gate in &core_gates {
        domain
            .try_reserve()
            .expect("core blocking reservation")
            .bundle(BlockingDrop(Arc::clone(gate)))
            .submit();
        wait_gate_entered(gate);
    }

    let elastic_gates: Vec<_> = (0..2)
        .map(|_| Arc::new((Mutex::new(GateState::default()), Condvar::new())))
        .collect();
    for gate in &elastic_gates {
        domain
            .try_reserve()
            .expect("queued elastic blocker")
            .bundle(BlockingDrop(Arc::clone(gate)))
            .submit();
    }
    let completed = Arc::new(AtomicBool::new(false));
    domain
        .try_reserve()
        .expect("queued terminal cleanup")
        .bundle(ObservedDrop(Arc::clone(&completed)))
        .submit();

    for gate in &elastic_gates {
        wait_gate_entered(gate);
    }
    wait_observed(&completed);

    for gate in core_gates.iter().chain(&elastic_gates) {
        release_gate(gate);
    }
}

#[test]
fn elastic_start_failure_keeps_the_charged_queue_and_later_submit_retries() {
    let spawner: Arc<WorkerSpawner> = Arc::new(move |worker, launcher| {
        if worker == 1 {
            return Err(io::Error::other("injected elastic startup failure"));
        }
        std::thread::Builder::new().spawn(move || {
            if let Ok(launch) = launcher.recv() {
                worker_loop(launch);
            }
        })
    });
    let domain =
        DropDomain::try_start_with(1, 3, spawner).expect("the core destructor worker must start");
    let gate = Arc::new((Mutex::new(GateState::default()), Condvar::new()));
    domain
        .try_reserve()
        .expect("blocking reservation")
        .bundle(BlockingDrop(Arc::clone(&gate)))
        .submit();
    wait_gate_entered(&gate);

    let first = Arc::new(AtomicBool::new(false));
    domain
        .try_reserve()
        .expect("first queued reservation")
        .bundle(ObservedDrop(Arc::clone(&first)))
        .submit();
    assert!(
        !first.load(Ordering::Acquire),
        "failed elastic startup must not destroy the queued value in the submitter"
    );

    let second = Arc::new(AtomicBool::new(false));
    domain
        .try_reserve()
        .expect("retry-triggering reservation")
        .bundle(ObservedDrop(Arc::clone(&second)))
        .submit();
    wait_observed(&first);
    wait_observed(&second);
    release_gate(&gate);
}

#[test]
fn live_and_starting_workers_never_exceed_internal_worker_limit() {
    let domain =
        DropDomain::try_start(MAX_WORKER_COUNT + 8).expect("the bounded domain must start");
    let mut gates = Vec::new();
    for _ in 0..MAX_WORKER_COUNT {
        let gate = Arc::new((Mutex::new(GateState::default()), Condvar::new()));
        domain
            .try_reserve()
            .expect("one reservation per worker")
            .bundle(BlockingDrop(Arc::clone(&gate)))
            .submit();
        wait_gate_entered(&gate);
        gates.push(gate);
    }

    let queued = Arc::new(AtomicBool::new(false));
    domain
        .try_reserve()
        .expect("ownership remains available above the worker ceiling")
        .bundle(ObservedDrop(Arc::clone(&queued)))
        .submit();
    let (live, _idle, starting) = domain.started_executor().workers.worker_counts();
    assert_eq!(live + starting, MAX_WORKER_COUNT);
    assert!(
        !queued.load(Ordering::Acquire),
        "a seventeenth worker must not be created while the bounded pool is blocked"
    );

    for gate in &gates {
        release_gate(gate);
    }
    wait_observed(&queued);
}

#[test]
fn core_start_failure_is_transactional() {
    let calls = Arc::new(AtomicUsize::new(0));
    let exited = Arc::new(AtomicUsize::new(0));
    let calls_for_spawn = Arc::clone(&calls);
    let exited_for_spawn = Arc::clone(&exited);
    let spawner: Arc<WorkerSpawner> = Arc::new(move |_worker, launcher| {
        let call = calls_for_spawn.fetch_add(1, Ordering::AcqRel);
        if call == 2 {
            return Err(io::Error::other("injected core startup failure"));
        }
        let exited = Arc::clone(&exited_for_spawn);
        std::thread::Builder::new().spawn(move || {
            let _ = launcher.recv();
            exited.fetch_add(1, Ordering::AcqRel);
        })
    });

    let error = match DropDomain::try_start_with(3, 4, spawner) {
        Ok(_) => panic!("the injected core startup failure must be returned"),
        Err(error) => error,
    };
    assert_eq!(error.worker(), 2);
    assert_eq!(error.into_source().kind(), io::ErrorKind::Other);
    assert_eq!(calls.load(Ordering::Acquire), 3);
    assert_eq!(
        exited.load(Ordering::Acquire),
        2,
        "every empty launcher started before failure must be joined"
    );
}

#[test]
fn idle_domain_drop_releases_worker_queue_ownership() {
    let domain =
        DropDomain::try_start_with(1, 1, system_spawner()).expect("the idle domain must start");
    let queue = Arc::downgrade(&domain.started_executor().workers);
    drop(domain);

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while queue.upgrade().is_some() {
        assert!(
            std::time::Instant::now() < deadline,
            "idle workers must not keep their domain alive"
        );
        std::thread::yield_now();
    }
}

#[test]
fn clean_burst_returns_to_only_the_transactional_core_workers() {
    let domain = DropDomain::try_start(64).expect("the burst domain must start");
    let completed = Arc::new(AtomicUsize::new(0));
    struct CountDrop(Arc<AtomicUsize>);
    impl Drop for CountDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    for _ in 0..32 {
        domain
            .try_reserve()
            .expect("burst reservation")
            .bundle(CountDrop(Arc::clone(&completed)))
            .submit();
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while completed.load(Ordering::Acquire) != 32 {
        assert!(std::time::Instant::now() < deadline, "burst must drain");
        std::thread::yield_now();
    }
    let retirement_deadline =
        std::time::Instant::now() + ELASTIC_IDLE_TIMEOUT + Duration::from_secs(2);
    loop {
        let (live, _idle, starting) = domain.started_executor().workers.worker_counts();
        if live == CORE_WORKER_COUNT && starting == 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < retirement_deadline,
            "clean work must leave only the transactional core workers; live={live}, starting={starting}"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn panicking_destructor_permanently_consumes_only_its_charged_slot() {
    struct PanickingDrop(Arc<AtomicBool>);

    impl Drop for PanickingDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
            panic!("injected destructor panic");
        }
    }

    let executor = test_executor(1, 2);
    let attempted = Arc::new(AtomicBool::new(false));
    let reported = Arc::new(AtomicBool::new(false));
    let mut bundle = executor
        .try_reserve()
        .expect("the poisoned slot")
        .bundle(PanickingDrop(Arc::clone(&attempted)));
    let held = executor.try_reserve().expect("the remaining slot");
    let mut waiter = Box::pin(executor.reserve());
    assert_pending_once(waiter.as_mut()).await;

    let attempted_for_report = Arc::clone(&attempted);
    let reported_by_worker = Arc::clone(&reported);
    bundle.set_panic_reporter(move |message| {
        assert!(
            attempted_for_report.load(Ordering::Acquire),
            "the destructor panic must be caught before reporting"
        );
        assert!(message.contains("injected destructor panic"));
        reported_by_worker.store(true, Ordering::Release);
    });
    bundle.submit();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !reported.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the worker must report the hostile destructor");
    wait_for_effective_capacity(&executor, 1).await;

    assert_pending_once(waiter.as_mut()).await;
    assert!(
        executor.try_reserve().is_err(),
        "the poisoned slot and the live reservation consume all capacity"
    );

    drop(held);
    let recovered = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("releasing a healthy slot must wake the waiter")
        .expect("one destructor panic must not close ownership admission");
    assert_eq!(executor.capacity.available(), 0);

    drop(recovered);
    assert_eq!(
        executor.capacity.available(),
        1,
        "only the panicking bundle's charged slot stays consumed"
    );
    let remaining = executor
        .try_reserve()
        .expect("the broker's healthy capacity remains usable");
    assert!(
        executor.try_reserve().is_err(),
        "the poisoned slot must never be reused"
    );
    drop(remaining);
    assert_eq!(executor.capacity.available(), 1);
    assert_eq!(executor.capacity.effective_capacity(), 1);
}

#[test]
fn poisoned_cleanup_reports_retirement_after_capacity_commits() {
    let domain = DropDomain::try_start(1).expect("the test domain must start");
    let (reported, report_rx) = mpsc::channel();
    domain.set_retirement_reporter(move |configured, effective, retired| {
        let _ = reported.send((configured, effective, retired));
    });
    let mut bundle = domain
        .try_reserve()
        .expect("the ownership unit to retire")
        .bundle(());
    bundle.poison();
    bundle.submit();

    assert_eq!(
        report_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("committed retirement must invoke its reporter"),
        (1, 0, 1)
    );
    let snapshot = domain.snapshot(true);
    assert_eq!(snapshot.effective_limit, Some(0));
    assert_eq!(snapshot.retired(), Some(1));
    assert!(!snapshot.admission_open);
}

#[tokio::test(flavor = "current_thread")]
async fn actor_cleanup_poison_consumes_only_its_charged_slot() {
    struct ObservedDrop(Arc<AtomicBool>);

    impl Drop for ObservedDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    let executor = test_executor(1, 2);
    let dropped = Arc::new(AtomicBool::new(false));
    let mut bundle = executor
        .try_reserve()
        .expect("the poisoned slot")
        .bundle(ObservedDrop(Arc::clone(&dropped)));
    let held = executor.try_reserve().expect("the remaining slot");
    let mut waiter = Box::pin(executor.reserve());
    assert_pending_once(waiter.as_mut()).await;

    bundle.poison();
    bundle.submit();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !dropped.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the worker must process the poisoned bundle");
    wait_for_effective_capacity(&executor, 1).await;

    assert_pending_once(waiter.as_mut()).await;
    drop(held);
    let recovered = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("releasing a healthy slot must wake the waiter")
        .expect("one poisoned bundle must not close ownership admission");
    assert_eq!(executor.capacity.available(), 0);

    drop(recovered);
    assert_eq!(
        executor.capacity.available(),
        1,
        "only the poisoned bundle's charged slot stays consumed"
    );
    let remaining = executor
        .try_reserve()
        .expect("the broker's healthy capacity remains usable");
    assert!(executor.try_reserve().is_err());
    drop(remaining);
    assert_eq!(executor.capacity.available(), 1);
    assert_eq!(executor.capacity.effective_capacity(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn retiring_the_last_slot_wakes_waiters_with_a_typed_error() {
    struct ObservedDrop(Arc<AtomicBool>);

    impl Drop for ObservedDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    let executor = test_executor(1, 1);
    let dropped = Arc::new(AtomicBool::new(false));
    let mut bundle = executor
        .try_reserve()
        .expect("the slot to retire")
        .bundle(ObservedDrop(Arc::clone(&dropped)));
    let mut waiter = Box::pin(executor.reserve());
    assert_pending_once(waiter.as_mut()).await;

    bundle.poison();
    bundle.submit();
    let result = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("retiring the last reachable slot must wake its waiter");
    assert!(result.is_err());
    assert!(dropped.load(Ordering::Acquire));
    assert_eq!(executor.capacity.available(), 0);
    assert_eq!(executor.capacity.effective_capacity(), 0);
    assert!(executor.try_reserve().is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn retirement_rejects_impossible_batch_but_preserves_healthy_progress() {
    let executor = test_executor(1, 3);
    let mut poisoned = executor
        .try_reserve()
        .expect("the slot to retire")
        .bundle(());
    let held_a = executor.try_reserve().expect("the first healthy slot");
    let held_b = executor.try_reserve().expect("the second healthy slot");

    let mut impossible = Box::pin(executor.reserve_many(3));
    assert_pending_once(impossible.as_mut()).await;
    let mut single = Box::pin(executor.reserve());
    assert_pending_once(single.as_mut()).await;

    poisoned.poison();
    poisoned.submit();
    let impossible = tokio::time::timeout(Duration::from_secs(1), impossible)
        .await
        .expect("retirement must wake an atomic request above effective capacity");
    assert!(impossible.is_err());
    assert_eq!(executor.capacity.effective_capacity(), 2);
    assert_pending_once(single.as_mut()).await;

    drop(held_a);
    let single = tokio::time::timeout(Duration::from_secs(1), single)
        .await
        .expect("a healthy released slot must still advance a feasible waiter")
        .expect("retirement must not close the healthy remainder");
    drop(single);
    drop(held_b);
    assert_eq!(executor.capacity.available(), 2);
    assert_eq!(executor.capacity.effective_capacity(), 2);
}
