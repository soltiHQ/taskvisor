//! Shared fixtures for controller engine tests.

pub(super) use crate::events::{Bus, Event, EventKind};
pub(super) use crate::{
    BoxTaskFuture, RuntimeError, Supervisor, Task, TaskContext, TaskFn, TaskId, TaskOutcome,
    TaskRef, TaskSpec,
    controller::{ControllerConfig, ControllerError, ControllerSpec},
};
pub(super) use futures_util::StreamExt;
pub(super) use std::{
    future::Future,
    num::NonZeroUsize,
    sync::{
        Arc, Condvar, Mutex as StdMutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
pub(super) use tokio::{
    sync::{Mutex, mpsc, oneshot},
    time::Instant,
};
pub(super) use tokio_util::sync::CancellationToken;

use crate::controller::engine::state::{
    AdmissionTransition, PendingSubmission, ReplaceAction, SlotState,
};
use crate::controller::engine::{
    Controller, ControllerState, OperationSet, Submission, TrackedOperations,
};

pub(super) struct SpawnBombTask;

impl Task for SpawnBombTask {
    fn spawn(&self, _ctx: TaskContext) -> BoxTaskFuture {
        panic!("a policy-rejected task must not spawn")
    }
}

pub(super) struct PanickingDropTask {
    pub(super) drops: Arc<AtomicUsize>,
}

impl Task for PanickingDropTask {
    fn spawn(&self, _ctx: TaskContext) -> BoxTaskFuture {
        Box::pin(std::future::pending())
    }
}

impl Drop for PanickingDropTask {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::AcqRel);
        panic!("injected task drop panic")
    }
}

#[derive(Default)]
pub(super) struct BlockingDropState {
    pub(super) entered: bool,
    pub(super) released: bool,
    pub(super) finished: bool,
}

pub(super) struct BlockingDropTask {
    pub(super) gate: Arc<(StdMutex<BlockingDropState>, Condvar)>,
}

impl Task for BlockingDropTask {
    fn spawn(&self, _ctx: TaskContext) -> BoxTaskFuture {
        Box::pin(std::future::pending())
    }
}

impl Drop for BlockingDropTask {
    fn drop(&mut self) {
        let (state, changed) = &*self.gate;
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        state.entered = true;
        changed.notify_all();
        while !state.released {
            state = changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
        state.finished = true;
        changed.notify_all();
    }
}

pub(super) struct ShutdownDropProbeTask {
    pub(super) controller: Weak<Controller>,
    pub(super) state_clean_at_drop: Arc<AtomicBool>,
    pub(super) drops: Arc<AtomicUsize>,
}

impl Task for ShutdownDropProbeTask {
    fn spawn(&self, _ctx: TaskContext) -> BoxTaskFuture {
        Box::pin(std::future::pending())
    }
}

impl Drop for ShutdownDropProbeTask {
    fn drop(&mut self) {
        let state_is_clean = self.controller.upgrade().is_some_and(|controller| {
            let state = controller.state();
            state.watchers.is_empty()
                && state.slots.is_empty()
                && state.queued_slots.is_empty()
                && state.capacity_pending.is_empty()
        });
        self.state_clean_at_drop
            .store(state_is_clean, Ordering::Release);
        self.drops.fetch_add(1, Ordering::AcqRel);
        panic!("injected task drop panic")
    }
}

pub(super) fn make_spec(name: &str) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(|_ctx: TaskContext| async { Ok(()) });
    TaskSpec::once(name, task)
}

pub(super) fn spawn_counting_controller_spec(
    name: &'static str,
    spawn_calls: &Arc<AtomicUsize>,
) -> ControllerSpec {
    let spawn_calls = Arc::clone(spawn_calls);
    let task: TaskRef = TaskFn::arc(move |_ctx: TaskContext| {
        spawn_calls.fetch_add(1, Ordering::AcqRel);
        async { Ok(()) }
    });
    ControllerSpec::queue(TaskSpec::once(name, task))
}

pub(super) fn assert_lazy_start_failure(error: ControllerError, worker: usize) {
    let ControllerError::ThreadStartFailed {
        component,
        worker: actual_worker,
        kind,
        raw_os_error,
    } = error
    else {
        panic!("expected a typed destructor-isolation startup failure, got {error:?}")
    };
    assert_eq!(component, "destructor_isolation");
    assert_eq!(actual_worker, worker);
    assert_eq!(kind, std::io::ErrorKind::Other);
    assert_eq!(raw_os_error, None);
    assert_eq!(error.as_label(), "controller_thread_start_failed");
}

pub(super) fn pending(id: TaskId, task_spec: TaskSpec) -> PendingSubmission {
    let task_name = task_spec.shared_name();
    PendingSubmission::new(id, task_name, owned_task_spec(task_spec))
}

pub(super) fn owned_task_spec(
    task_spec: TaskSpec,
) -> crate::core::deferred_drop::OwnedTask<TaskSpec> {
    let retained = task_spec.task().clone();
    let reservation = crate::core::deferred_drop::test_reservation();
    crate::core::deferred_drop::OwnedTask::new(task_spec, retained, reservation)
}

pub(super) fn isolated_owned_task_spec(
    task_spec: TaskSpec,
) -> crate::core::deferred_drop::OwnedTask<TaskSpec> {
    let retained = task_spec.task().clone();
    let reservation = crate::core::deferred_drop::isolated_test_reservation();
    crate::core::deferred_drop::OwnedTask::new(task_spec, retained, reservation)
}

pub(super) fn owned_controller_spec(
    spec: ControllerSpec,
) -> crate::core::deferred_drop::OwnedTask<ControllerSpec> {
    let retained = spec.task_spec().task().clone();
    let reservation = crate::core::deferred_drop::test_reservation();
    crate::core::deferred_drop::OwnedTask::new(spec, retained, reservation)
}

pub(super) fn isolated_owned_controller_spec(
    spec: ControllerSpec,
) -> crate::core::deferred_drop::OwnedTask<ControllerSpec> {
    let retained = spec.task_spec().task().clone();
    let reservation = crate::core::deferred_drop::isolated_test_reservation();
    crate::core::deferred_drop::OwnedTask::new(spec, retained, reservation)
}

pub(super) fn with_controller_panic_reporter<T>(
    mut owned: crate::core::deferred_drop::OwnedTask<T>,
    bus: &Bus,
) -> crate::core::deferred_drop::OwnedTask<T> {
    let bus = bus.clone();
    owned.cleanup.set_panic_reporter(move |message| {
        bus.publish(Event::runtime_failure(
            "controller",
            format!("task_drop_panicked: {message}"),
        ));
    });
    owned
}

pub(super) fn slot_arc_name() -> Arc<str> {
    Arc::from("s")
}

pub(super) fn drain_events(
    events: &mut tokio::sync::broadcast::Receiver<Arc<Event>>,
) -> Vec<Arc<Event>> {
    let mut drained = Vec::new();
    while let Ok(event) = events.try_recv() {
        drained.push(event);
    }
    drained
}

pub(super) async fn drain_until_runtime_failure(
    events: &mut tokio::sync::broadcast::Receiver<Arc<Event>>,
    needle: &str,
) -> Vec<Arc<Event>> {
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut drained = Vec::new();
        loop {
            drained.extend(drain_events(events));
            if drained.iter().any(|event| {
                event.kind == EventKind::RuntimeFailure
                    && event
                        .reason
                        .as_deref()
                        .is_some_and(|reason| reason.contains(needle))
            }) {
                break drained;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the deferred panic reporter must publish its diagnostic")
}

pub(super) fn assert_rejection_parity(event: &Event, id: TaskId, outcome: &TaskOutcome) {
    let TaskOutcome::Rejected { kind, reason, .. } = outcome else {
        panic!("expected a rejected task outcome, got {outcome:?}");
    };
    assert_eq!(event.kind, EventKind::ControllerRejected);
    assert_eq!(event.id, Some(id));
    assert_eq!(event.rejection_kind, Some(*kind));
    assert_eq!(event.outcome_kind, Some(crate::TaskOutcomeKind::Rejected));
    assert_eq!(event.reason.as_deref(), Some(reason.as_ref()));
}

pub(super) async fn receive_oneshot<T>(receiver: oneshot::Receiver<T>, context: &str) -> T {
    tokio::time::timeout(Duration::from_secs(2), receiver)
        .await
        .unwrap_or_else(|_| panic!("{context} timed out"))
        .unwrap_or_else(|_| panic!("{context} sender was dropped"))
}

pub(super) fn admitting_slot(owner: TaskId) -> SlotState {
    let mut slot = SlotState::new();
    assert!(slot.begin_admission(owner, Instant::now()));
    slot
}

pub(super) fn running_slot(owner: TaskId) -> SlotState {
    let mut slot = admitting_slot(owner);
    assert_eq!(
        slot.confirm_admission(owner, Instant::now()),
        AdmissionTransition::Running
    );
    slot
}

pub(super) fn terminating_slot(owner: TaskId) -> SlotState {
    let mut slot = running_slot(owner);
    assert_eq!(
        slot.request_replacement(Instant::now()),
        ReplaceAction::RemoveNow(owner)
    );
    slot
}

pub(super) async fn abort_and_drain<T: 'static>(operations: &mut OperationSet<T>) {
    operations.clear();
}

pub(super) fn tracked_operations(ctrl: &Controller) -> TrackedOperations {
    TrackedOperations::new(
        ctrl.supervisor.clone(),
        ctrl.config.admission_capacity().get(),
    )
}

pub(super) async fn handle_submission_fully(
    ctrl: &Controller,
    submission: Submission,
    operations: &mut TrackedOperations,
) {
    ctrl.handle_submission(submission, operations).await;
}

pub(super) fn make_controller(config: ControllerConfig, bus: Bus) -> Controller {
    let drop_domain = crate::core::deferred_drop::TestReservationSource::new(64).domain();
    make_controller_with_domain(config, bus, drop_domain)
}

pub(super) fn make_controller_with_domain(
    config: ControllerConfig,
    bus: Bus,
    drop_domain: crate::core::deferred_drop::DropDomain,
) -> Controller {
    let (tx, rx) = mpsc::channel(config.queue_capacity().get());
    Controller {
        config,
        supervisor: Weak::new(),
        bus,
        drop_domain,
        shutdown_token: CancellationToken::new(),
        state: StdMutex::new(ControllerState::default()),
        tx,
        rx: StdMutex::new(Some(rx)),
        shutting_down: AtomicBool::new(false),
        task: OnceLock::new(),
    }
}

pub(super) fn waiting_spec(name: &'static str) -> TaskSpec {
    let task: TaskRef = TaskFn::arc(|ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok(())
    });
    TaskSpec::restartable(name, task)
}

pub(super) async fn start_controller_loop(
    ctrl: &Arc<Controller>,
    token: &CancellationToken,
) -> tokio::task::JoinHandle<Result<(), &'static str>> {
    let receiver = ctrl
        .take_command_receiver()
        .expect("controller loop receiver present");
    let runner_ctrl = Arc::clone(ctrl);
    let runner_token = token.clone();
    tokio::spawn(async move { runner_ctrl.run_inner(runner_token, receiver).await })
}

pub(super) async fn stop_controller_loop(
    token: CancellationToken,
    runner: tokio::task::JoinHandle<Result<(), &'static str>>,
) {
    token.cancel();
    tokio::time::timeout(Duration::from_secs(1), runner)
        .await
        .expect("controller loop must stop after cancellation")
        .expect("controller loop task must not panic")
        .expect("controller loop must exit cleanly");
}

pub(super) async fn poll_until<F, Fut>(within: Duration, mut cond: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + within;
    loop {
        if cond().await {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
