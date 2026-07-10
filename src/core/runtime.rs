//! # Supervisor runtime core.
//!
//! [`SupervisorCore`] owns the runtime components behind the public [`Supervisor`](super::supervisor::Supervisor) facade.
//!
//! It wires together:
//! - registry command channel,
//! - [`Registry`],
//! - event bus,
//! - subscriber fan-out,
//! - alive-task tracker,
//! - runtime shutdown token.
//!
//! ## Planes
//!
//! ```text
//! Management plane:
//!   SupervisorHandle -> SupervisorCore -> mpsc -> Registry
//!
//! Event plane:
//!   runtime components -> Bus -> subscriber_listener
//!                               -> AliveTracker
//!                               -> SubscriberSet
//!
//! Shutdown plane:
//!   OS signal / handle.shutdown -> drain_with_grace
//!                               -> cancel registry tasks
//!                               -> join listeners
//!                               -> close subscribers
//! ```
//!
//! Add/remove commands use the management plane.
//! They are not delivered through the lossy event bus.
//!
//! Events are used for observability, alive snapshots, and subscriber delivery.
//! Event consumers may lag.
//!
//! ## Modes
//!
//! ```text
//! start()
//!   starts subscriber listener and registry listener
//!
//! run(tasks)
//!   starts listeners
//!   adds initial tasks
//!   waits for registration confirmation best-effort
//!   waits for OS shutdown signal or natural completion
//!
//! shutdown()
//!   cancels all tasks
//!   waits up to grace
//!   force-aborts tasks that do not stop
//!   joins internal listeners
//! ```
//!
//! ## Rules
//!
//! - New task admission closes once shutdown begins.
//! - Registry membership is keyed by `TaskId`.
//! - `run()` is single-shot. A second call returns `RuntimeError::AlreadyRunning`.
//! - `snapshot` and `is_alive` are best-effort views from the alive tracker.
//! - `start()` is idempotent.

use std::sync::atomic::{AtomicBool, Ordering};
use std::{sync::Arc, time::Duration};
use tokio::{sync::broadcast, sync::mpsc, time::timeout};
use tokio_util::sync::CancellationToken;

use crate::core::{
    alive::AliveTracker,
    registry::{Registry, RegistryCommand},
};
use crate::{
    core::SupervisorConfig,
    error::RuntimeError,
    events::{Bus, Event, EventKind},
    identity::TaskId,
    subscribers::SubscriberSet,
    tasks::TaskSpec,
};

/// Runtime implementation behind the public [`Supervisor`](super::supervisor::Supervisor).
///
/// This type is controller-agnostic.
/// The public facade may compose it with an optional controller, but the core itself only manages
/// registry commands, events, subscribers, alive tracking, and shutdown.
pub(crate) struct SupervisorCore {
    cfg: SupervisorConfig,
    pub(super) bus: Bus,
    subs: Arc<SubscriberSet>,
    alive: Arc<AliveTracker>,
    registry: Arc<Registry>,
    runtime_token: CancellationToken,
    started: AtomicBool,
    running: AtomicBool,
    shutting_down: AtomicBool,
    cmd_tx: mpsc::UnboundedSender<RegistryCommand>,
    subscriber_handle: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for SupervisorCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupervisorCore")
            .field("cfg", &self.cfg)
            .field("started", &self.started.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl SupervisorCore {
    /// Creates a ready-to-share runtime core.
    ///
    /// Used by the builder after all runtime components have been wired.
    pub(crate) fn new_internal(
        cfg: SupervisorConfig,
        bus: Bus,
        subs: Arc<SubscriberSet>,
        alive: Arc<AliveTracker>,
        registry: Arc<Registry>,
        runtime_token: CancellationToken,
        cmd_tx: mpsc::UnboundedSender<RegistryCommand>,
    ) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            bus,
            subs,
            alive,
            registry,
            runtime_token,
            started: AtomicBool::new(false),
            running: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            cmd_tx,
            subscriber_handle: std::sync::Mutex::new(None),
        })
    }

    /// Returns true once shutdown has started and new task admission is closed.
    pub(crate) fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }

    /// Marks the runtime as shutting down.
    ///
    /// This is idempotent and closes the admission gate for future adds.
    fn mark_shutting_down(&self) {
        self.shutting_down.store(true, Ordering::Release);
    }

    /// Queues a task add command and returns its minted [`TaskId`].
    ///
    /// `Ok(id)` means the command was sent to the registry listener.
    /// Registration may still fail later, for example because the task name already exists.
    pub(crate) fn add_task(&self, spec: TaskSpec) -> Result<TaskId, RuntimeError> {
        self.add_task_inner(TaskId::next(), spec, None)
    }

    /// Queues a task add command under a pre-minted identity.
    ///
    /// Used by the controller so a submission keeps the same [`TaskId`] from admission through registry registration.
    ///
    /// Unlike [`add_task`](Self::add_task), this **hands the watcher `done` back** in the error
    /// tuple on failure instead of dropping it, so the controller can resolve the submission's
    /// waiter with `Rejected` rather than leaving it to observe a canceled oneshot.
    #[cfg(feature = "controller")]
    pub(crate) fn add_task_with_id_watched(
        &self,
        id: TaskId,
        spec: TaskSpec,
        done: Option<crate::core::registry::OutcomeTx>,
    ) -> Result<TaskId, (RuntimeError, Option<crate::core::registry::OutcomeTx>)> {
        if self.is_shutting_down() {
            return Err((RuntimeError::ShuttingDown, done));
        }
        self.bus.publish(
            Event::new(EventKind::TaskAddRequested)
                .with_task(spec.task().name())
                .with_id(id),
        );
        match self.cmd_tx.send(RegistryCommand::Add(id, spec, done)) {
            Ok(()) => Ok(id),
            Err(mpsc::error::SendError(cmd)) => {
                let done = match cmd {
                    RegistryCommand::Add(_, _, done) => done,
                    RegistryCommand::Remove(_) => None,
                };
                Err((RuntimeError::ShuttingDown, done))
            }
        }
    }

    /// Queues a watched task add command.
    ///
    /// Returns the minted [`TaskId`] and a receiver that resolves to the final [`TaskOutcome`](crate::TaskOutcome)
    /// if the task is registered and later terminates.
    pub(crate) fn add_task_watched(
        &self,
        spec: TaskSpec,
    ) -> Result<(TaskId, tokio::sync::oneshot::Receiver<crate::TaskOutcome>), RuntimeError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let id = self.add_task_inner(TaskId::next(), spec, Some(tx))?;
        Ok((id, rx))
    }

    /// Sends the registry `Add` command and publishes `TaskAddRequested`.
    ///
    /// Returns `ShuttingDown` if admission is already closed or the registry command channel is closed.
    fn add_task_inner(
        &self,
        id: TaskId,
        spec: TaskSpec,
        done: Option<crate::core::registry::OutcomeTx>,
    ) -> Result<TaskId, RuntimeError> {
        if self.is_shutting_down() {
            return Err(RuntimeError::ShuttingDown);
        }
        self.bus.publish(
            Event::new(EventKind::TaskAddRequested)
                .with_task(spec.task().name())
                .with_id(id),
        );
        self.cmd_tx
            .send(RegistryCommand::Add(id, spec, done))
            .map_err(|_| RuntimeError::ShuttingDown)?;
        Ok(id)
    }

    /// Queues a remove command by runtime identity.
    ///
    /// This is fire-and-forget.
    /// `Ok(())` means the command was sent, not that the task existed or has already stopped.
    pub(crate) fn remove(&self, id: TaskId) -> Result<(), RuntimeError> {
        self.bus
            .publish(Event::new(EventKind::TaskRemoveRequested).with_id(id));
        self.cmd_tx
            .send(RegistryCommand::Remove(id))
            .map_err(|_| RuntimeError::ShuttingDown)
    }

    /// Returns registered tasks as `(id, label)` pairs from the registry.
    pub(crate) async fn list_tasks(&self) -> Vec<(TaskId, Arc<str>)> {
        self.registry.list().await
    }

    /// Returns true if `id` is currently registered.
    pub(crate) async fn contains_id(&self, id: TaskId) -> bool {
        self.registry.contains(id).await
    }

    /// Resolves a label to the identity currently holding it (if any).
    pub(crate) async fn id_for_label(&self, name: &str) -> Option<TaskId> {
        self.registry.id_for_label(name).await
    }

    /// Returns a new bus receiver for event subscription.
    pub(crate) fn subscribe_bus(&self) -> broadcast::Receiver<Arc<Event>> {
        self.bus.subscribe()
    }

    /// Starts runtime listeners without blocking.
    ///
    /// This starts:
    /// - the subscriber listener,
    /// - the registry listener.
    ///
    /// Safe to call more than once. Later calls are no-ops.
    pub(crate) fn start(&self) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        self.subscriber_listener();
        self.registry.clone().spawn_listener();
    }

    /// Runs a static task set until OS shutdown signal or natural completion.
    ///
    /// This starts the runtime listeners, queues the initial tasks, waits for their
    /// registration confirmations best-effort, then drives shutdown/natural
    /// completion.
    ///
    /// Single-shot: a second or concurrent call returns [`RuntimeError::AlreadyRunning`].
    pub(crate) async fn run(&self, tasks: Vec<TaskSpec>) -> Result<(), RuntimeError> {
        if self.running.swap(true, Ordering::AcqRel) {
            return Err(RuntimeError::AlreadyRunning);
        }
        self.start();

        if !tasks.is_empty() {
            let mut rx = self.bus.subscribe();
            let mut pending_ids = Vec::with_capacity(tasks.len());
            for spec in tasks {
                pending_ids.push(self.add_task(spec)?);
            }
            self.wait_tasks_registered(&mut rx, &pending_ids).await;
        }
        self.drive_shutdown().await
    }

    /// Best-effort wait for initial task registration events.
    ///
    /// Waits for `TaskAdded` or `TaskAddFailed` for every initial task id.
    /// If the bus receiver lags, falls back to registry membership.
    ///
    /// This wait has a fixed backstop and currently does not return an error when the backstop expires.
    async fn wait_tasks_registered(
        &self,
        rx: &mut broadcast::Receiver<Arc<Event>>,
        ids: &[TaskId],
    ) {
        let mut pending: Vec<TaskId> = ids.to_vec();

        let confirm = async {
            while !pending.is_empty() {
                match rx.recv().await {
                    Ok(ev)
                        if matches!(ev.kind, EventKind::TaskAdded | EventKind::TaskAddFailed) =>
                    {
                        if let Some(id) = ev.id {
                            pending.retain(|p| *p != id);
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let mut still = Vec::new();
                        for id in std::mem::take(&mut pending) {
                            if !self.registry.contains(id).await {
                                still.push(id);
                            }
                        }
                        pending = still;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        };
        const CONFIRM_BACKSTOP: Duration = Duration::from_secs(5);
        let _ = timeout(CONFIRM_BACKSTOP, confirm).await;
    }

    /// Initiates explicit graceful shutdown.
    ///
    /// Publishes `ShutdownRequested`, drains tasks with grace, cancels the runtime token, joins internal listeners,
    /// and closes subscriber workers within their configured timeout.
    pub(crate) async fn shutdown(&self) -> Result<(), RuntimeError> {
        self.bus.publish(Event::new(EventKind::ShutdownRequested));
        let res = self.drain_with_grace().await;
        self.runtime_token.cancel();
        self.registry.join_listener().await;
        self.join_subscriber_listener().await;
        self.subs.close().await;
        res
    }

    /// Returns a best-effort sorted list of task names currently marked alive.
    pub(crate) async fn snapshot(&self) -> Vec<Arc<str>> {
        self.alive.snapshot().await
    }

    /// Returns true if any task with this name is currently marked alive.
    ///
    /// This is a best-effort label query from the alive tracker.
    pub(crate) async fn is_alive(&self, name: &str) -> bool {
        self.alive.is_alive(name).await
    }

    /// Cancels a task by identity and waits for `TaskRemoved`.
    pub(crate) async fn cancel(&self, id: TaskId) -> Result<bool, RuntimeError> {
        self.cancel_with_timeout(id, self.cfg.grace).await
    }

    /// Cancels a task with an explicit confirmation window.
    ///
    /// `wait_for` controls how long this call waits for `TaskRemoved`.
    /// It does not change the registry's force-abort grace period.
    pub(crate) async fn cancel_with_timeout(
        &self,
        id: TaskId,
        wait_for: Duration,
    ) -> Result<bool, RuntimeError> {
        let mut rx = self.bus.subscribe();
        if !self.registry.contains(id).await {
            return Ok(false);
        }

        self.bus.publish(
            Event::new(EventKind::TaskRemoveRequested)
                .with_id(id)
                .with_reason("manual_cancel"),
        );
        self.cmd_tx
            .send(RegistryCommand::Remove(id))
            .map_err(|_| RuntimeError::ShuttingDown)?;
        self.wait_task_removed(&mut rx, id, wait_for).await
    }

    /// Applies one event to alive tracking and subscriber fan-out.
    async fn distribute(alive: &AliveTracker, set: &SubscriberSet, ev: Arc<Event>) {
        alive.update(&ev).await;
        set.emit_arc(ev);
    }

    /// Drains retained events from a bus receiver.
    ///
    /// Used when the subscriber listener is shutting down.
    /// Broadcast lag gaps are skipped so the retained tail can still be delivered to alive tracking and subscribers.
    async fn drain_pending(
        rx: &mut broadcast::Receiver<Arc<Event>>,
        alive: &AliveTracker,
        set: &SubscriberSet,
    ) {
        loop {
            match rx.try_recv() {
                Ok(ev) => Self::distribute(alive, set, ev).await,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    }

    /// Waits for the subscriber listener task to finish.
    ///
    /// If the listener was never started, this is a no-op.
    fn subscriber_listener(&self) {
        let mut rx = self.bus.subscribe();
        let set = Arc::clone(&self.subs);
        let alive = Arc::clone(&self.alive);
        let registry = Arc::clone(&self.registry);
        let rt = self.runtime_token.clone();

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;

                    msg = rx.recv() => match msg {
                        Ok(arc_ev) => {
                            if let Err(panic) = crate::core::panic_guard::guarded(
                                Self::distribute(&alive, &set, arc_ev),
                            )
                            .await
                            {
                                set.emit_arc(Arc::new(Event::subscriber_panicked(
                                    "subscriber_listener",
                                    format!("listener panic: {panic}"),
                                )));
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            let arc_e = Arc::new(Event::subscriber_overflow(
                                "subscriber_listener",
                                format!("lagged({skipped})"),
                            ));
                            alive.update(&arc_e).await;
                            set.emit_arc(arc_e);

                            let live: std::collections::HashSet<TaskId> = registry
                                .list()
                                .await
                                .into_iter()
                                .map(|(id, _)| id)
                                .collect();
                            alive.reconcile(&live).await;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    },

                    _ = rt.cancelled() => {
                        Self::drain_pending(&mut rx, &alive, &set).await;
                        break;
                    }
                }
            }
        });

        *self.subscriber_handle.lock().unwrap() = Some(handle);
    }

    /// Awaits the subscriber listener.
    async fn join_subscriber_listener(&self) {
        let handle = self.subscriber_handle.lock().unwrap().take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    /// Drives static-mode completion.
    ///
    /// Waits for either:
    /// - an OS shutdown signal,
    /// - natural completion when the registry becomes empty.
    ///
    /// Both paths join registry/subscriber listeners and close subscribers before returning.
    async fn drive_shutdown(&self) -> Result<(), RuntimeError> {
        let res = tokio::select! {
            sig = crate::core::shutdown::wait_for_shutdown_signal() => self.on_shutdown_signal(sig).await,
            _ = self.registry.wait_until_empty() => {
                let stuck = self.registry.wait_joins_within(self.cfg.grace).await;
                if stuck.is_empty() {
                    self.bus
                        .publish(Event::new(EventKind::AllStoppedWithinGrace));
                    Ok(())
                } else {
                    self.bus.publish(Event::new(EventKind::GraceExceeded));
                    Err(RuntimeError::GraceExceeded { grace: self.cfg.grace, stuck })
                }
            }
        };
        self.mark_shutting_down();
        self.runtime_token.cancel();
        self.registry.join_listener().await;
        self.join_subscriber_listener().await;
        self.subs.close().await;
        res
    }

    /// Handles the result of OS shutdown-signal setup/waiting.
    ///
    /// A real signal publishes `ShutdownRequested` and starts graceful drain.
    /// Signal setup errors are returned as [`RuntimeError::SignalSetupFailed`] and are not treated as shutdown requests.
    async fn on_shutdown_signal(&self, res: std::io::Result<()>) -> Result<(), RuntimeError> {
        match res {
            Ok(()) => {
                self.bus.publish(Event::new(EventKind::ShutdownRequested));
                self.drain_with_grace().await
            }
            Err(e) => Err(RuntimeError::SignalSetupFailed { source: e }),
        }
    }

    /// Cancels tasks and waits for them within the configured grace window.
    ///
    /// This drains registered tasks first, then waits for detached join reporters using the remaining grace.
    /// Tasks/joiners that do not finish are returned as `GraceExceeded` stuck labels.
    async fn drain_with_grace(&self) -> Result<(), RuntimeError> {
        self.mark_shutting_down();
        let grace = self.cfg.grace;
        let deadline = tokio::time::Instant::now() + grace;
        let mut stuck = self.registry.cancel_all_within(grace).await;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        stuck.extend(self.registry.wait_joins_within(remaining).await);
        if stuck.is_empty() {
            self.bus
                .publish(Event::new(EventKind::AllStoppedWithinGrace));
            Ok(())
        } else {
            self.bus.publish(Event::new(EventKind::GraceExceeded));
            Err(RuntimeError::GraceExceeded { grace, stuck })
        }
    }

    /// Waits for a task to terminate.
    ///
    /// Success is normally observed through `TaskRemoved`.
    /// If the bus receiver lags or closes, falls back to registry termination state.
    async fn wait_task_removed(
        &self,
        rx: &mut broadcast::Receiver<Arc<Event>>,
        id: TaskId,
        wait_for: Duration,
    ) -> Result<bool, RuntimeError> {
        let wait_for_event = async {
            loop {
                match rx.recv().await {
                    Ok(ev) if matches!(ev.kind, EventKind::TaskRemoved) && ev.id == Some(id) => {
                        return true;
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if self.registry.is_terminated(id).await {
                            return true;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return self.registry.is_terminated(id).await;
                    }
                }
            }
        };

        match timeout(wait_for, wait_for_event).await {
            Ok(true) => Ok(true),
            Ok(false) | Err(_) => {
                if self.registry.is_terminated(id).await {
                    Ok(true)
                } else {
                    Err(RuntimeError::TaskRemoveTimeout {
                        id,
                        timeout: wait_for,
                    })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subscribers::Subscribe;
    use std::sync::Mutex;

    /// Subscriber that records a clone of every event it receives.
    ///
    /// A large queue keeps the recorder from lagging itself, so tests assert on the bus
    /// contents, not on subscriber-side drops.
    struct RecordingSub {
        seen: Arc<Mutex<Vec<Event>>>,
    }
    impl RecordingSub {
        /// Returns the subscriber plus a shared handle to the events it records.
        fn new() -> (Arc<Self>, Arc<Mutex<Vec<Event>>>) {
            let seen = Arc::new(Mutex::new(Vec::new()));
            (
                Arc::new(Self {
                    seen: Arc::clone(&seen),
                }),
                seen,
            )
        }
    }
    impl Subscribe for RecordingSub {
        fn on_event(&self, e: &Event) {
            self.seen.lock().unwrap().push(e.clone());
        }
        fn name(&self) -> &str {
            "recorder"
        }
        fn queue_capacity(&self) -> usize {
            8192
        }
    }

    fn core(cfg: SupervisorConfig) -> Arc<SupervisorCore> {
        core_with_subs(cfg, Vec::new())
    }

    fn core_with_subs(
        cfg: SupervisorConfig,
        subs: Vec<Arc<dyn crate::subscribers::Subscribe>>,
    ) -> Arc<SupervisorCore> {
        let bus = Bus::new(cfg.bus_capacity_clamped());
        let subs = Arc::new(SubscriberSet::new(subs, bus.clone()));
        let token = CancellationToken::new();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let registry = Registry::new(bus.clone(), token.clone(), None, cfg.grace, cmd_rx);
        let alive = Arc::new(AliveTracker::new());
        SupervisorCore::new_internal(cfg, bus, subs, alive, registry, token, cmd_tx)
    }

    #[tokio::test]
    async fn subscriber_listener_reports_bus_lag_as_overflow() {
        let (recorder, seen) = RecordingSub::new();

        let cfg = SupervisorConfig {
            bus_capacity: 2,
            ..Default::default()
        };
        let core = core_with_subs(cfg, vec![recorder]);
        core.start();

        for i in 0..500 {
            core.bus
                .publish(Event::new(EventKind::TaskStarting).with_task(format!("f{i}")));
        }

        let saw_lag = timeout(Duration::from_secs(2), async {
            loop {
                let hit = seen.lock().unwrap().iter().any(|e| {
                    e.kind == EventKind::SubscriberOverflow
                        && e.reason
                            .as_deref()
                            .is_some_and(|r| r.starts_with("lagged("))
                });
                if hit {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or(false);

        let _ = core.shutdown().await;
        assert!(
            saw_lag,
            "subscriber_listener must report bus lag as SubscriberOverflow(lagged(n))"
        );
    }

    #[tokio::test]
    async fn drain_pending_delivers_retained_tail_after_a_lag_gap() {
        let (recorder, seen) = RecordingSub::new();

        let bus = Bus::new(2);
        let mut rx = bus.subscribe();
        let set = Arc::new(SubscriberSet::new(vec![recorder], bus.clone()));
        let alive = AliveTracker::new();

        for i in 0..5 {
            bus.publish(Event::new(EventKind::TaskStarting).with_task(format!("t{i}")));
        }

        SupervisorCore::drain_pending(&mut rx, &alive, &set).await;
        set.close().await;

        let delivered = seen.lock().unwrap();
        assert!(
            delivered
                .iter()
                .any(|e| e.kind == EventKind::TaskStarting && e.task.as_deref() == Some("t4")),
            "newest retained event must reach subscribers despite a lag gap"
        );
    }

    #[tokio::test]
    async fn natural_completion_publishes_all_stopped_within_grace() {
        use crate::{TaskContext, TaskFn, TaskRef};

        let (recorder, seen) = RecordingSub::new();
        let core = core_with_subs(SupervisorConfig::default(), vec![recorder]);

        let task: TaskRef = TaskFn::arc("done", |_ctx: TaskContext| async move { Ok(()) });
        let res = timeout(Duration::from_secs(5), core.run(vec![TaskSpec::once(task)])).await;
        assert!(
            matches!(res, Ok(Ok(()))),
            "natural completion must return Ok, got {res:?}"
        );

        assert!(
            seen.lock()
                .unwrap()
                .iter()
                .any(|e| e.kind == EventKind::AllStoppedWithinGrace),
            "natural-completion success must publish a terminal verdict (AllStoppedWithinGrace)"
        );
    }

    #[tokio::test]
    async fn run_is_single_shot() {
        let core = core(SupervisorConfig::default());

        let first = timeout(Duration::from_secs(5), core.run(vec![])).await;
        assert!(
            matches!(first, Ok(Ok(()))),
            "first run must succeed, got {first:?}"
        );

        let second = core.run(vec![]).await;
        assert!(
            matches!(second, Err(RuntimeError::AlreadyRunning)),
            "second run() must return AlreadyRunning, got {second:?}"
        );
    }

    #[tokio::test]
    async fn add_is_rejected_once_shutting_down() {
        use crate::{TaskContext, TaskFn, TaskRef};

        let core = core(SupervisorConfig::default());
        core.start();

        let early: TaskRef = TaskFn::arc("early", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        assert!(core.add_task(TaskSpec::restartable(early)).is_ok());

        core.mark_shutting_down();

        let late: TaskRef = TaskFn::arc("late", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        let res = core.add_task(TaskSpec::restartable(late));
        assert!(
            matches!(res, Err(RuntimeError::ShuttingDown)),
            "add() after shutdown began must be rejected, got {res:?}"
        );

        let _ = core.shutdown().await;
    }

    #[cfg(feature = "controller")]
    #[tokio::test]
    async fn add_task_with_id_watched_returns_watcher_on_failure() {
        use crate::{TaskContext, TaskFn, TaskRef};

        let core = core(SupervisorConfig::default());
        core.mark_shutting_down(); // close the admission gate so the add fails

        let (tx, rx) = tokio::sync::oneshot::channel();
        let task: TaskRef = TaskFn::arc("x", |_ctx: TaskContext| async { Ok(()) });

        let res = core.add_task_with_id_watched(TaskId::next(), TaskSpec::once(task), Some(tx));
        match res {
            Err((RuntimeError::ShuttingDown, Some(returned))) => {
                returned
                    .send(crate::TaskOutcome::Rejected {
                        reason: Arc::from("rejected"),
                    })
                    .expect("returned watcher must still be live");
                assert!(matches!(rx.await, Ok(crate::TaskOutcome::Rejected { .. })));
            }
            other => panic!("add must hand the watcher back on failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn signal_setup_error_surfaces_as_runtime_error_not_shutdown() {
        let core = core(SupervisorConfig::default());
        let mut rx = core.bus.subscribe();

        let err = std::io::Error::other("signal registration failed");
        let out = core.on_shutdown_signal(Err(err)).await;

        assert!(
            matches!(out, Err(RuntimeError::SignalSetupFailed { .. })),
            "a signal-setup error must surface as SignalSetupFailed, got {out:?}"
        );

        let mut saw_shutdown = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev.kind, EventKind::ShutdownRequested) {
                saw_shutdown = true;
            }
        }
        assert!(
            !saw_shutdown,
            "a signal-setup error must NOT masquerade as a shutdown request"
        );
    }

    #[tokio::test]
    async fn real_signal_publishes_shutdown_requested() {
        let core = core(SupervisorConfig::default());
        let mut rx = core.bus.subscribe();

        let out = core.on_shutdown_signal(Ok(())).await;
        assert!(out.is_ok(), "a real signal drains gracefully: {out:?}");

        let mut saw_shutdown = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev.kind, EventKind::ShutdownRequested) {
                saw_shutdown = true;
            }
        }
        assert!(saw_shutdown, "a real signal must publish ShutdownRequested");
    }

    #[tokio::test]
    async fn wait_task_removed_survives_bus_lag() {
        use crate::{TaskContext, TaskFn, TaskRef};

        let cfg = SupervisorConfig {
            bus_capacity: 8,
            ..Default::default()
        };
        let core = core(cfg);
        core.start();

        let t: TaskRef = TaskFn::arc("laggy", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        let id = core
            .add_task(TaskSpec::restartable(t))
            .expect("add accepted");

        let registered = timeout(Duration::from_secs(2), async {
            loop {
                if core.contains_id(id).await {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        registered.expect("task must register");

        let mut lagged_rx = core.subscribe_bus();
        let mut observer = core.subscribe_bus();

        core.remove(id).expect("remove should be accepted");

        let observed = timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(ev) = observer.recv().await
                    && ev.kind == EventKind::TaskRemoved
                    && ev.id == Some(id)
                {
                    return;
                }
            }
        })
        .await;
        observed.expect("TaskRemoved must be observed by a healthy receiver");

        for _ in 0..32 {
            core.bus
                .publish(Event::new(EventKind::TaskStarting).with_task("noise"));
        }

        let res = timeout(
            Duration::from_secs(1),
            core.wait_task_removed(&mut lagged_rx, id, Duration::from_millis(300)),
        )
        .await
        .expect("wait_task_removed must not hang");
        assert!(
            matches!(res, Ok(true)),
            "a lagged receiver must fall back to runtime state instead of reporting \
             a spurious TaskRemoveTimeout, got {res:?}"
        );

        let _ = core.shutdown().await;
    }
}
