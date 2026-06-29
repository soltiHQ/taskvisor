//! # SupervisorCore: the runtime guts behind the [`Supervisor`](super::supervisor::Supervisor) facade.
//!
//! `SupervisorCore` owns the runtime components and orchestrates task execution from spawn to graceful shutdown.
//!
//! ## Architecture
//! ```text
//! SupervisorCore::start()           // non-blocking setup
//!     ├──► subscriber_listener()
//!     │     ├──► updates AliveTracker
//!     │     └──► distributes to SubscriberSet
//!     └──► Registry.spawn_listener()
//!           ├──► cmd_rx: Add(id, spec)  → spawn actor
//!           ├──► cmd_rx: Remove(id)     → cancel task
//!           ├──► bus_rx: ActorExhausted → cleanup
//!           └──► bus_rx: ActorDead      → cleanup
//!
//! SupervisorCore::run(tasks)        // start() + block until shutdown
//!     ├──► start()
//!     ├──► subscribe to bus (before publishing!)
//!     ├──► add initial tasks
//!     ├──► wait for each task's add outcome (TaskAdded/TaskAddFailed)
//!     └──► drive_shutdown()
//!
//! SupervisorCore::shutdown()        // explicit graceful stop
//!     └──► drain_with_grace (cancel tasks, join within grace, force-abort stragglers)
//! ```
//!
//! ## Rules
//! - Each task has individual cancellation token (not shared with runtime_token)
//! - Graceful shutdown cancels all tasks and waits for registry to drain
//! - Registry auto-cleanup via ActorExhausted/ActorDead events
//! - All operations are event-driven (idempotent)
//! - Registry tracks all active tasks by `TaskId` (with a label index for name lookups)

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

/// Controller-agnostic runtime owned by the [`Supervisor`](super::supervisor::Supervisor) facade.
///
/// See the [module documentation](self) for the architecture.
pub(crate) struct SupervisorCore {
    cfg: SupervisorConfig,
    pub(super) bus: Bus,
    subs: Arc<SubscriberSet>,
    alive: Arc<AliveTracker>,
    registry: Arc<Registry>,
    runtime_token: CancellationToken,
    started: AtomicBool,
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
    /// Internal constructor used by the builder; returns a ready-to-share `Arc`.
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
            shutting_down: AtomicBool::new(false),
            cmd_tx,
            subscriber_handle: std::sync::Mutex::new(None),
        })
    }

    /// Returns `true` once shutdown has begun (the admission gate is closed).
    pub(crate) fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }

    /// Marks the runtime as shutting down (idempotent). Closes the admission gate.
    fn mark_shutting_down(&self) {
        self.shutting_down.store(true, Ordering::Release);
    }

    /// Adds a new task to the runtime, returning its minted [`TaskId`].
    pub(crate) fn add_task(&self, spec: TaskSpec) -> Result<TaskId, RuntimeError> {
        self.add_task_inner(TaskId::next(), spec, None)
    }

    /// Core primitive: add a task under a **pre-minted** identity with an optional outcome sender.
    #[cfg(feature = "controller")]
    pub(crate) fn add_task_with_id_watched(
        &self,
        id: TaskId,
        spec: TaskSpec,
        done: Option<crate::core::registry::OutcomeTx>,
    ) -> Result<TaskId, RuntimeError> {
        self.add_task_inner(id, spec, done)
    }

    /// Adds a new task and returns a `oneshot` receiver resolving to its final [`TaskOutcome`](crate::TaskOutcome).
    pub(crate) fn add_task_watched(
        &self,
        spec: TaskSpec,
    ) -> Result<(TaskId, tokio::sync::oneshot::Receiver<crate::TaskOutcome>), RuntimeError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let id = self.add_task_inner(TaskId::next(), spec, Some(tx))?;
        Ok((id, rx))
    }

    /// Publishes `TaskAddRequested` and sends the `Add` command with an optional outcome watcher.
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

    /// Removes a task from the runtime, by its runtime identity.
    pub(crate) fn remove(&self, id: TaskId) -> Result<(), RuntimeError> {
        self.bus
            .publish(Event::new(EventKind::TaskRemoveRequested).with_id(id));
        self.cmd_tx
            .send(RegistryCommand::Remove(id))
            .map_err(|_| RuntimeError::ShuttingDown)
    }

    /// Returns currently active tasks as `(id, label)` pairs from the registry.
    pub(crate) async fn list_tasks(&self) -> Vec<(TaskId, Arc<str>)> {
        self.registry.list().await
    }

    /// Checks if a task with the given identity is registered.
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

    /// Starts the runtime event loop without blocking.
    ///
    /// Spawns:
    /// - subscriber listener (distributes events to subscribers)
    /// - registry listener (task lifecycle management)
    pub(crate) fn start(&self) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        self.subscriber_listener();
        self.registry.clone().spawn_listener();
    }

    /// Runs task specifications until completion or shutdown signal.
    pub(crate) async fn run(&self, tasks: Vec<TaskSpec>) -> Result<(), RuntimeError> {
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

    /// Waits until every initially-added task has been processed by the registry.
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

    /// Initiates graceful shutdown: cancels all tasks and waits up to `grace` for them to stop.
    pub(crate) async fn shutdown(&self) -> Result<(), RuntimeError> {
        self.bus.publish(Event::new(EventKind::ShutdownRequested));
        let res = self.drain_with_grace().await;
        self.runtime_token.cancel();
        self.registry.join_listener().await;
        self.join_subscriber_listener().await;
        self.subs.close().await;
        res
    }

    /// Returns sorted list of currently alive task names.
    pub(crate) async fn snapshot(&self) -> Vec<Arc<str>> {
        self.alive.snapshot().await
    }

    /// Check whether a given task is currently alive.
    pub(crate) async fn is_alive(&self, name: &str) -> bool {
        self.alive.is_alive(name).await
    }

    /// Cancel a task by identity and wait for confirmation (`TaskRemoved`).
    pub(crate) async fn cancel(&self, id: TaskId) -> Result<bool, RuntimeError> {
        self.cancel_with_timeout(id, self.cfg.grace).await
    }

    /// Cancel with explicit timeout.
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

    /// Listens to the internal event bus and distributes each received [`Event`] to all active subscribers.
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
                            alive.update(&arc_ev).await;
                            set.emit_arc(arc_ev);
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
                        while let Ok(arc_ev) = rx.try_recv() {
                            alive.update(&arc_ev).await;
                            set.emit_arc(arc_ev);
                        }
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

    /// Waits for either shutdown signal or natural completion of all tasks.
    async fn drive_shutdown(&self) -> Result<(), RuntimeError> {
        let res = tokio::select! {
            sig = crate::core::shutdown::wait_for_shutdown_signal() => self.on_shutdown_signal(sig).await,
            _ = self.registry.wait_until_empty() => {
                // Natural completion: tasks finished; drain their cleanup joiners while the
                // subscriber listener is alive so each final `TaskRemoved` is delivered. A
                // joiner that overruns grace is surfaced as `GraceExceeded`, not swallowed.
                let stuck = self.registry.wait_joins_within(self.cfg.grace).await;
                if stuck.is_empty() {
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

    /// Handles the outcome of [`wait_for_shutdown_signal`](crate::core::shutdown::wait_for_shutdown_signal).
    async fn on_shutdown_signal(&self, res: std::io::Result<()>) -> Result<(), RuntimeError> {
        match res {
            Ok(()) => {
                self.bus.publish(Event::new(EventKind::ShutdownRequested));
                self.drain_with_grace().await
            }
            Err(e) => Err(RuntimeError::SignalSetupFailed { source: e }),
        }
    }

    /// Cancels all tasks and waits up to `grace` for them to stop, force-aborting stragglers.
    async fn drain_with_grace(&self) -> Result<(), RuntimeError> {
        self.mark_shutting_down();
        let grace = self.cfg.grace;
        let deadline = tokio::time::Instant::now() + grace;
        // Map-resident tasks: cancel + join within grace; `stuck` are those force-aborted.
        let mut stuck = self.registry.cancel_all_within(grace).await;
        // Detached joiners (e.g. a `remove` just before shutdown) live outside the map; wait
        // for them within the *remaining* grace, before `runtime_token.cancel()`, so their
        // final `TaskRemoved` is delivered while the subscriber listener is still alive. Any
        // that overrun are folded into `stuck` by label, so the verdict names who is stuck.
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

    /// Waits up to `wait_for` for a task to **actually terminate**, signalled by its `TaskRemoved` event.
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

    /// Builds a started-on-demand `SupervisorCore` directly (mirrors the builder's core wiring).
    fn core(cfg: SupervisorConfig) -> Arc<SupervisorCore> {
        core_with_subs(cfg, Vec::new())
    }

    /// Like [`core`], but wires the given subscribers into the runtime.
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
        use crate::subscribers::Subscribe;
        use std::sync::Mutex;

        // Records the reason of every SubscriberOverflow. A large queue so the
        // recorder never lags itself — only the listener's bus receiver should.
        struct OverflowRecorder {
            reasons: Arc<Mutex<Vec<String>>>,
        }
        impl Subscribe for OverflowRecorder {
            fn on_event(&self, e: &Event) {
                if e.kind == EventKind::SubscriberOverflow
                    && let Some(r) = e.reason.as_deref()
                {
                    self.reasons.lock().unwrap().push(r.to_string());
                }
            }
            fn name(&self) -> &str {
                "overflow-recorder"
            }
            fn queue_capacity(&self) -> usize {
                8192
            }
        }

        let reasons = Arc::new(Mutex::new(Vec::<String>::new()));
        let recorder = Arc::new(OverflowRecorder {
            reasons: Arc::clone(&reasons),
        });

        // Tiny bus so the listener's own receiver lags under a synchronous burst.
        let cfg = SupervisorConfig {
            bus_capacity: 2,
            ..Default::default()
        };
        let core = core_with_subs(cfg, vec![recorder]);
        core.start();

        // Publish far beyond capacity before the listener can drain: its recv() lags,
        // and the listener must synthesize SubscriberOverflow { reason: "lagged(n)" }.
        for i in 0..500 {
            core.bus
                .publish(Event::new(EventKind::TaskStarting).with_task(format!("f{i}")));
        }

        let saw_lag = timeout(Duration::from_secs(2), async {
            loop {
                if reasons
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|r| r.starts_with("lagged("))
                {
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
            "subscriber_listener must report bus lag as SubscriberOverflow(lagged(n)); saw {:?}",
            reasons.lock().unwrap()
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
