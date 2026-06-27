//! # Supervisor: orchestrates task actors and graceful shutdown.
//!
//! The [`Supervisor`] owns the runtime components and orchestrates task execution lifecycle from spawn to graceful termination.
//!
//! - Spawn task actors via event-driven registry
//! - Dynamically add/remove tasks at runtime via events
//! - Perform graceful shutdown with configurable grace period
//! - Enforce global concurrency limits via optional semaphore
//! - Track alive tasks for stuck detection during shutdown
//! - Distribute events to subscribers via [`SubscriberSet`]
//! - Handle OS termination signals
//!
//! ## Architecture
//! ```text
//! Supervisor::start()           // non-blocking setup
//!     ├──► subscriber_listener()
//!     │     ├──► updates AliveTracker
//!     │     └──► distributes to SubscriberSet
//!     └──► Registry.spawn_listener()
//!           ├──► cmd_rx: Add(id, spec)  → spawn actor
//!           ├──► cmd_rx: Remove(id)     → cancel task
//!           ├──► bus_rx: ActorExhausted → cleanup
//!           └──► bus_rx: ActorDead      → cleanup
//!
//! Supervisor::run(tasks)        // start() + block until shutdown
//!     ├──► start()
//!     ├──► subscribe to bus (before publishing!)
//!     ├──► add initial tasks
//!     ├──► wait for each task's add outcome (TaskAdded/TaskAddFailed)
//!     └──► drive_shutdown()
//!           ├──► wait for signal or empty registry
//!           └──► drain_with_grace (cancel tasks, join within grace, force-abort stragglers)
//!
//! Supervisor::shutdown()        // explicit graceful stop
//!     └──► drain_with_grace (cancel tasks, join within grace, force-abort stragglers)
//! ```
//!
//! ## Runtime task management
//! ```text
//! add_task(spec)                           // mints TaskId, returns it to caller
//!     ├──► Bus.publish(TaskAddRequested)   // observability (non-blocking)
//!     ├──► cmd_tx.send(Add(id, spec))      // guaranteed delivery (mpsc)
//!     └──► Registry.spawn_listener
//!           ├──► spawn actor
//!           ├──► registry.insert(handle)
//!           └──► Bus.publish(TaskAdded)
//!
//! remove(id)
//!     ├──► Bus.publish(TaskRemoveRequested) // observability (fire-and-forget)
//!     ├──► cmd_tx.send(Remove(id))          // guaranteed delivery (mpsc)
//!     └──► Registry.spawn_listener
//!           ├──► registry.remove(id) + cancel token
//!           ├──► await actor finish
//!           └──► Bus.publish(TaskRemoved)
//!
//! Actor finishes
//!     ├──► Bus.publish(ActorExhausted/ActorDead)
//!     └──► Registry.event_listener
//!           ├──► registry.remove(id)
//!           └──► Bus.publish(TaskRemoved)
//! ```
//!
//! ## Rules
//! - Each task has individual cancellation token (not shared with runtime_token)
//! - Graceful shutdown cancels all tasks and waits for registry to drain
//! - Registry auto-cleanup via ActorExhausted/ActorDead events
//! - All operations are event-driven (idempotent)
//! - Registry tracks all active tasks by `TaskId` (with a label index for name lookups)
//!
//! ## Example
//! ```rust,no_run
//! use std::time::Duration;
//! use taskvisor::TaskContext;
//!
//! use taskvisor::{SupervisorConfig, Supervisor, TaskSpec, TaskFn, RestartPolicy, BackoffPolicy};
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let cfg = SupervisorConfig::default();
//!     let sup = Supervisor::new(cfg, Vec::new());
//!
//!     let task = TaskFn::arc("ticker", |ctx: TaskContext| async move {
//!         while !ctx.is_cancelled() {
//!             tokio::time::sleep(Duration::from_millis(250)).await;
//!         }
//!         Ok(())
//!     });
//!
//!     let spec = TaskSpec::new(
//!         task,
//!         RestartPolicy::Never,
//!         BackoffPolicy::default(),
//!         Some(Duration::from_secs(2)),
//!     );
//!
//!     sup.run(vec![spec]).await?;
//!     Ok(())
//! }
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::{sync::Arc, time::Duration};
use tokio::{sync::Notify, sync::broadcast, sync::mpsc, time::timeout};
use tokio_util::sync::CancellationToken;

#[cfg(feature = "controller")]
use tokio::sync::OnceCell;

use crate::core::{
    alive::AliveTracker,
    builder::SupervisorBuilder,
    registry::{Registry, RegistryCommand},
};
use crate::{
    core::SupervisorConfig,
    error::RuntimeError,
    events::{Bus, Event, EventKind},
    identity::TaskId,
    subscribers::{Subscribe, SubscriberSet},
    tasks::TaskSpec,
};

/// Orchestrates task actors, event delivery, and graceful shutdown.
///
/// ## Two usage modes
///
/// **Static**: run a known set of tasks until completion or Ctrl+C:
/// ```rust,no_run
/// # use taskvisor::prelude::*;
/// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
/// sup.run(vec![/* specs */]).await?;
/// # Ok(()) }
/// ```
///
/// **Dynamic**: add/remove/cancel tasks at runtime via [`SupervisorHandle`]:
/// ```rust,no_run
/// # use taskvisor::prelude::*;
/// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
/// let handle = sup.serve();
/// // handle.add(...), handle.cancel(...), handle.shutdown().await
/// # Ok(()) }
/// ```
///
/// # Also
///
/// - [`SupervisorHandle`](crate::SupervisorHandle) - runtime management API returned by [`serve`](Self::serve)
/// - [`SupervisorBuilder`](crate::SupervisorBuilder) - step-by-step construction
/// - [`SupervisorConfig`] - configuration knobs
///
/// [`SupervisorHandle`]: crate::SupervisorHandle
pub struct Supervisor {
    cfg: SupervisorConfig,
    bus: Bus,
    subs: Arc<SubscriberSet>,
    alive: Arc<AliveTracker>,
    registry: Arc<Registry>,
    ready: Arc<Notify>,
    runtime_token: CancellationToken,
    started: AtomicBool,
    cmd_tx: mpsc::UnboundedSender<RegistryCommand>,
    subscriber_handle: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,

    #[cfg(feature = "controller")]
    pub(super) controller: OnceCell<Arc<crate::controller::Controller>>,
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor")
            .field("cfg", &self.cfg)
            .field("started", &self.started.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Supervisor {
    /// Internal constructor used by builder (not public API).
    pub(crate) fn new_internal(
        cfg: SupervisorConfig,
        bus: Bus,
        subs: Arc<SubscriberSet>,
        alive: Arc<AliveTracker>,
        registry: Arc<Registry>,
        runtime_token: CancellationToken,
        cmd_tx: mpsc::UnboundedSender<RegistryCommand>,
    ) -> Self {
        Self {
            cfg,
            bus,
            subs,
            alive,
            registry,
            runtime_token,
            ready: Arc::new(Notify::new()),
            started: AtomicBool::new(false),
            cmd_tx,
            subscriber_handle: std::sync::Mutex::new(None),

            #[cfg(feature = "controller")]
            controller: OnceCell::new(),
        }
    }

    /// Creates a new supervisor with the given config and subscribers (maybe empty).
    pub fn new(cfg: SupervisorConfig, subscribers: Vec<Arc<dyn Subscribe>>) -> Arc<Self> {
        Self::builder(cfg).with_subscribers(subscribers).build()
    }

    /// Creates a builder for constructing a Supervisor.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use taskvisor::{SupervisorConfig, Supervisor};
    ///
    /// let sup = Supervisor::builder(SupervisorConfig::default())
    ///     .with_subscribers(vec![])
    ///     .build();
    /// ```
    pub fn builder(cfg: SupervisorConfig) -> SupervisorBuilder {
        SupervisorBuilder::new(cfg)
    }

    /// Returns a [`SupervisorHandle`] for dynamic task management.
    ///
    /// Starts the event loop (listeners) and returns a handle through which you can add, remove, or cancel tasks and trigger shutdown.
    ///
    /// This is the **only** way to manage tasks dynamically. Use [`run()`] for static task sets.
    ///
    /// [`SupervisorHandle`]: crate::SupervisorHandle
    /// [`run()`]: Self::run
    pub fn serve(self: &Arc<Self>) -> super::handle::SupervisorHandle {
        self.start();
        super::handle::SupervisorHandle::new(Arc::clone(self))
    }

    /// Adds a new task to the supervisor at runtime, returning its minted [`TaskId`].
    pub(crate) fn add_task(&self, spec: TaskSpec) -> Result<TaskId, RuntimeError> {
        self.add_task_inner(TaskId::next(), spec, None)
    }

    /// Adds a task under a **pre-minted** identity with an optional outcome watcher.
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

    /// Removes a task from the supervisor at runtime, by its runtime identity.
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

    /// Starts the supervisor event loop without blocking.
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
        self.ready.notify_waiters();
    }

    /// Runs task specifications until completion or shutdown signal.
    ///
    /// Steps:
    /// - Start event loop (via `start`)
    /// - Subscribe to bus **before** publishing (guarantees no missed events)
    /// - Publish TaskAddRequested for initial tasks
    /// - Wait for each task's add outcome (`TaskAdded`/`TaskAddFailed`) before driving shutdown
    /// - Wait for shutdown signal or all tasks to exit
    pub async fn run(&self, tasks: Vec<TaskSpec>) -> Result<(), RuntimeError> {
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
    ///
    /// Tasks that do not stop cooperatively within the configured grace period are force-terminated;
    /// see [`drain_with_grace`](Self::drain_with_grace).
    pub(crate) async fn shutdown(&self) -> Result<(), RuntimeError> {
        self.bus.publish(Event::new(EventKind::ShutdownRequested));
        let res = self.drain_with_grace().await;
        self.runtime_token.cancel();
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

    /// Submits a task to the controller (if enabled), returning the pre-minted [`TaskId`].
    #[cfg(feature = "controller")]
    pub(crate) async fn submit(
        &self,
        spec: crate::controller::ControllerSpec,
    ) -> Result<TaskId, crate::controller::ControllerError> {
        match self.controller.get() {
            Some(ctrl) => ctrl.handle().submit(spec).await,
            None => Err(crate::controller::ControllerError::NotConfigured),
        }
    }

    /// Tries to submit a task without blocking, returning the pre-minted [`TaskId`].
    #[cfg(feature = "controller")]
    pub(crate) fn try_submit(
        &self,
        spec: crate::controller::ControllerSpec,
    ) -> Result<TaskId, crate::controller::ControllerError> {
        match self.controller.get() {
            Some(ctrl) => ctrl.handle().try_submit(spec),
            None => Err(crate::controller::ControllerError::NotConfigured),
        }
    }

    /// Submits a task to the controller and returns a watcher for its final outcome.
    #[cfg(feature = "controller")]
    pub(crate) async fn submit_and_watch(
        &self,
        spec: crate::controller::ControllerSpec,
    ) -> Result<
        (TaskId, tokio::sync::oneshot::Receiver<crate::TaskOutcome>),
        crate::controller::ControllerError,
    > {
        match self.controller.get() {
            Some(ctrl) => ctrl.handle().submit_and_watch(spec).await,
            None => Err(crate::controller::ControllerError::NotConfigured),
        }
    }

    /// Returns a point-in-time snapshot of the controller's slots, or `None` if no controller is configured.
    #[cfg(feature = "controller")]
    pub(crate) async fn controller_snapshot(
        &self,
    ) -> Option<crate::controller::ControllerSnapshot> {
        match self.controller.get() {
            Some(ctrl) => Some(ctrl.snapshot().await),
            None => None,
        }
    }

    /// Listens to the internal event bus and distributes each received [`Event`] to all active subscribers.
    ///
    /// The listener also updates the [`AliveTracker`] before distributing them, to keep the latest per-task state in sync.
    ///
    /// On `Lagged`, it creates a [`EventKind::SubscriberOverflow`] event and emits it **directly to subscribers**,
    /// bypassing the bus - to avoid reinforcing backpressure loops while still reporting the loss.
    ///
    /// On runtime shutdown the loop drains any buffered events to subscribers and exits.
    fn subscriber_listener(&self) {
        let mut rx = self.bus.subscribe();
        let set = Arc::clone(&self.subs);
        let alive = Arc::clone(&self.alive);
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
                            let e = Event::new(EventKind::SubscriberOverflow)
                                .with_task("subscriber_listener")
                                .with_reason(format!("lagged({skipped})"));

                            let arc_e = Arc::new(e);
                            alive.update(&arc_e).await;
                            set.emit_arc(arc_e);
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
            _ = self.registry.wait_until_empty() => Ok(()),
        };
        self.runtime_token.cancel();
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
        let grace = self.cfg.grace;
        let stuck = self.registry.cancel_all_within(grace).await;
        if stuck.is_empty() {
            self.bus
                .publish(Event::new(EventKind::AllStoppedWithinGrace));
            Ok(())
        } else {
            self.bus.publish(Event::new(EventKind::GraceExceeded));
            Err(RuntimeError::GraceExceeded { grace, stuck })
        }
    }

    /// Waits up to `wait_for` for a task to **actually terminate**, signalled by its `TaskRemoved`
    /// event (published by the registry only after the actor's `JoinHandle` completes).
    ///
    /// The `TaskRemoved` event may be lost to broadcast lag, that's why on `Lagged`/`Closed` and on timeout this falls back to the registry's termination state
    /// (`is_terminated`: not registered **and** not awaiting join) instead of reporting a spurious `TaskRemoveTimeout`.
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
    use crate::{TaskContext, TaskFn, TaskRef};

    #[tokio::test]
    async fn signal_setup_error_surfaces_as_runtime_error_not_shutdown() {
        let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
        let mut rx = sup.bus.subscribe();

        let err = std::io::Error::other("signal registration failed");
        let out = sup.on_shutdown_signal(Err(err)).await;

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
        let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
        let mut rx = sup.bus.subscribe();

        let out = sup.on_shutdown_signal(Ok(())).await;
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
    async fn shutdown_force_terminates_noncooperative_task_within_grace() {
        let cfg = SupervisorConfig {
            grace: Duration::from_millis(200),
            ..Default::default()
        };
        let sup = Supervisor::new(cfg, vec![]);
        let handle = sup.serve();

        let stubborn: TaskRef = TaskFn::arc("stubborn", |_ctx: TaskContext| async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(())
        });
        handle
            .add_and_wait(TaskSpec::once(stubborn), Duration::from_secs(1))
            .await
            .expect("task should register");

        let res = tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("shutdown must return within grace, not block on the stuck task");

        match res {
            Err(RuntimeError::GraceExceeded { stuck, .. }) => {
                assert!(
                    stuck.iter().any(|n| &**n == "stubborn"),
                    "stuck set must list the non-cooperative task, got {stuck:?}"
                );
            }
            other => panic!("expected GraceExceeded listing the stuck task, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn shutdown_cooperative_task_returns_ok() {
        let cfg = SupervisorConfig {
            grace: Duration::from_secs(5),
            ..Default::default()
        };
        let sup = Supervisor::new(cfg, vec![]);
        let handle = sup.serve();

        let good: TaskRef = TaskFn::arc("good", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        handle
            .add_and_wait(TaskSpec::restartable(good), Duration::from_secs(1))
            .await
            .expect("task should register");

        let res = timeout(Duration::from_secs(5), handle.shutdown())
            .await
            .expect("cooperative shutdown must not hang");
        assert!(
            res.is_ok(),
            "cooperative shutdown should be Ok, got {res:?}"
        );
    }

    #[tokio::test]
    async fn add_and_wait_duplicate_name_returns_already_exists() {
        let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
        let handle = sup.serve();

        let make = || -> TaskRef {
            TaskFn::arc("dup", |ctx: TaskContext| async move {
                ctx.cancelled().await;
                Ok(())
            })
        };
        handle
            .add_and_wait(TaskSpec::restartable(make()), Duration::from_secs(1))
            .await
            .expect("first add should succeed");

        let res = handle
            .add_and_wait(TaskSpec::restartable(make()), Duration::from_secs(1))
            .await;
        assert!(
            matches!(res, Err(RuntimeError::TaskAlreadyExists { .. })),
            "duplicate add must return TaskAlreadyExists, got {res:?}"
        );

        let _ = handle.shutdown().await;
    }

    #[tokio::test]
    async fn wait_task_removed_survives_bus_lag() {
        let cfg = SupervisorConfig {
            bus_capacity: 8,
            ..Default::default()
        };
        let sup = Supervisor::new(cfg, vec![]);
        let handle = sup.serve();

        let t: TaskRef = TaskFn::arc("laggy", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        let id = handle
            .add_and_wait(TaskSpec::restartable(t), Duration::from_secs(1))
            .await
            .expect("add should succeed");

        let mut lagged_rx = sup.subscribe_bus();
        let mut observer = sup.subscribe_bus();

        sup.remove(id).expect("remove should be accepted");

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
            sup.bus
                .publish(Event::new(EventKind::TaskStarting).with_task("noise"));
        }

        let res = timeout(
            Duration::from_secs(1),
            sup.wait_task_removed(&mut lagged_rx, id, Duration::from_millis(300)),
        )
        .await
        .expect("wait_task_removed must not hang");
        assert!(
            matches!(res, Ok(true)),
            "a lagged receiver must fall back to runtime state instead of reporting \
             a spurious TaskRemoveTimeout, got {res:?}"
        );

        let _ = handle.shutdown().await;
    }

    #[tokio::test]
    async fn cancel_reports_removed_then_absent() {
        let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
        let handle = sup.serve();

        let t: TaskRef = TaskFn::arc("c", |ctx: TaskContext| async move {
            ctx.cancelled().await;
            Ok(())
        });
        let id = handle
            .add_and_wait(TaskSpec::restartable(t), Duration::from_secs(1))
            .await
            .expect("add should succeed");

        let removed = handle.cancel(id).await.expect("cancel should not error");
        assert!(
            removed,
            "cancelling an existing task should report removed=true"
        );

        let absent = handle.cancel(id).await.expect("cancel should not error");
        assert!(!absent, "cancelling a missing task should report false");

        let _ = handle.shutdown().await;
    }
}
