//! # Supervisor: orchestrates task actors and graceful shutdown.
//!
//! The [`Supervisor`] owns the runtime components (event bus, subscribers, alive tracker, registry)
//! and orchestrates task execution lifecycle from spawn to graceful termination.
//!
//! - Spawn task actors via event-driven registry
//! - Dynamically add/remove tasks at runtime via events
//! - Perform graceful shutdown with configurable grace period
//! - Enforce global concurrency limits via optional semaphore
//! - Track alive tasks for stuck detection during shutdown
//! - Fan-out events to subscribers via [`SubscriberSet`]
//! - Handle OS termination signals
//!
//! ## Architecture
//! ```text
//! Supervisor::start()           // non-blocking setup
//!     ├──► subscriber_listener()
//!     │     ├──► updates AliveTracker
//!     │     └──► fans out to SubscriberSet
//!     └──► Registry.spawn_listener()
//!           ├──► TaskAddRequested → spawn actor
//!           ├──► TaskRemoveRequested → cancel task
//!           ├──► ActorExhausted → cleanup
//!           └──► ActorDead → cleanup
//!
//! Supervisor::run(tasks)        // start() + block until shutdown
//!     ├──► start()
//!     ├──► subscribe to bus (before publishing!)
//!     ├──► add initial tasks
//!     ├──► wait for N TaskAdded confirmations
//!     └──► drive_shutdown()
//!           ├──► wait for signal or empty registry
//!           ├──► cancel all tasks
//!           └──► wait_all_with_grace
//!
//! Supervisor::shutdown()        // explicit graceful stop
//!     ├──► cancel all tasks
//!     └──► wait_all_with_grace
//! ```
//!
//! ## Runtime task management
//! ```text
//! add_task(spec)
//!     ├──► Bus.publish(TaskAddRequested + spec)
//!     └──► Registry.event_listener
//!           ├──► spawn actor
//!           ├──► registry.insert(handle)
//!           └──► Bus.publish(TaskAdded)
//!
//! remove_task(name)
//!     ├──► Bus.publish(TaskRemoveRequested + name)
//!     └──► Registry.event_listener
//!           ├──► registry.remove(name) + cancel token
//!           ├──► await actor finish
//!           └──► Bus.publish(TaskRemoved)
//!
//! Actor finishes
//!     ├──► Bus.publish(ActorExhausted/ActorDead)
//!     └──► Registry.event_listener
//!           ├──► registry.remove(name)
//!           └──► Bus.publish(TaskRemoved)
//! ```
//!
//! ## Rules
//! - Registry tracks all active tasks by name
//! - Each task has individual cancellation token (not shared with runtime_token)
//! - Registry auto-cleanup via ActorExhausted/ActorDead events
//! - Graceful shutdown cancels all tasks and waits for registry to drain
//! - All operations are event-driven (idempotent)
//!
//! ## Example
//! ```rust
//! use std::time::Duration;
//!
//! use tokio_util::sync::CancellationToken;
//!
//! use taskvisor::{SupervisorConfig, Supervisor, TaskSpec, TaskFn, RestartPolicy, BackoffPolicy};
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let cfg = SupervisorConfig::default();
//!     let sup = Supervisor::new(cfg, Vec::new());
//!
//!     let task = TaskFn::arc("ticker", |ctx: CancellationToken| async move {
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
use tokio::{sync::Notify, sync::broadcast, time::timeout};
use tokio_util::sync::CancellationToken;

#[cfg(feature = "controller")]
use tokio::sync::OnceCell;

use crate::core::{alive::AliveTracker, builder::SupervisorBuilder, registry::Registry};
use crate::{
    core::SupervisorConfig,
    error::RuntimeError,
    events::{Bus, Event, EventKind},
    subscribers::{Subscribe, SubscriberSet},
    tasks::TaskSpec,
};

/// Orchestrates task actors, event delivery, and graceful shutdown.
///
/// - Spawns and supervises task actors via event-driven registry
/// - Provides runtime task management (add/remove tasks dynamically)
/// - Fans out events to subscribers (non-blocking)
/// - Handles graceful shutdown on OS signals
/// - Tracks alive tasks for stuck detection
/// - Enforces global concurrency limits
pub struct Supervisor {
    cfg: SupervisorConfig,
    bus: Bus,
    subs: Arc<SubscriberSet>,
    alive: Arc<AliveTracker>,
    registry: Arc<Registry>,
    ready: Arc<Notify>,
    runtime_token: CancellationToken,
    started: AtomicBool,

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

    /// Waits until supervisor is fully initialized and ready to accept submissions.
    ///
    /// Uses register-before-check pattern: the `Notified` future is created
    /// **before** checking `started`, so no notification is lost between
    /// the check and the wait.
    pub async fn wait_ready(&self) {
        loop {
            let notified = self.ready.notified();
            if self.started.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    /// Adds a new task to the supervisor at runtime.
    ///
    /// Publishes `TaskAddRequested` with the spec to the bus.
    /// Registry listener will spawn the actor.
    pub fn add_task(&self, spec: TaskSpec) -> Result<(), RuntimeError> {
        self.bus.publish(
            Event::new(EventKind::TaskAddRequested)
                .with_task(spec.task().name())
                .with_spec(spec),
        );
        Ok(())
    }

    /// Removes a task from the supervisor at runtime.
    ///
    /// Publishes `TaskRemoveRequested` to the bus.
    /// Registry listener will cancel and remove the task.
    pub fn remove_task(&self, name: &str) -> Result<(), RuntimeError> {
        self.bus
            .publish(Event::new(EventKind::TaskRemoveRequested).with_task(name));
        Ok(())
    }

    /// Returns a sorted list of currently active task names from the registry.
    pub async fn list_tasks(&self) -> Vec<Arc<str>> {
        self.registry.list().await
    }

    /// Starts the supervisor event loop without blocking.
    ///
    /// Spawns:
    /// - subscriber listener (event fan-out)
    /// - registry listener (task lifecycle management)
    ///
    /// Use [`wait_ready`] after this to ensure readiness before submitting.
    pub fn start(&self) {
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
    /// - Start event loop (via [`start`])
    /// - Subscribe to bus **before** publishing (guarantees no missed events)
    /// - Publish TaskAddRequested for initial tasks
    /// - Wait for N `TaskAdded` confirmations from registry listener
    /// - Wait for shutdown signal or all tasks to exit
    pub async fn run(&self, tasks: Vec<TaskSpec>) -> Result<(), RuntimeError> {
        self.start();

        let expected = tasks.len();
        if expected > 0 {
            let mut rx = self.bus.subscribe();
            for spec in tasks {
                self.add_task(spec)?;
            }
            self.wait_tasks_registered(&mut rx, expected).await;
        }
        self.drive_shutdown().await
    }

    /// Waits until registry listener confirms N tasks were registered.
    ///
    /// Subscribes to bus **before** publishing, so no `TaskAdded` events can be missed.
    /// On `Lagged`, stops waiting — tasks were registered but we lost the events;
    /// `drive_shutdown` will handle the rest.
    async fn wait_tasks_registered(
        &self,
        rx: &mut broadcast::Receiver<Arc<Event>>,
        expected: usize,
    ) {
        let mut registered = 0usize;
        while registered < expected {
            match rx.recv().await {
                Ok(ev) if ev.kind == EventKind::TaskAdded => {
                    registered += 1;
                }
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => break,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }

    /// Initiates graceful shutdown: cancels all tasks and waits for them to stop.
    ///
    /// Use this when embedding the supervisor (via [`start`]) instead of relying on the OS signal handler inside [`run`].
    pub async fn shutdown(&self) -> Result<(), RuntimeError> {
        self.bus.publish(Event::new(EventKind::ShutdownRequested));
        self.registry.cancel_all().await;
        let res = self.wait_all_with_grace().await;
        self.runtime_token.cancel();
        self.subs.close().await;
        res
    }

    /// Returns sorted list of currently alive task names.
    pub async fn snapshot(&self) -> Vec<Arc<str>> {
        self.alive.snapshot().await
    }

    /// Check whether a given task is currently alive.
    pub async fn is_alive(&self, name: &str) -> bool {
        self.alive.is_alive(name).await
    }

    /// Cancel a task by name and wait for confirmation (`TaskRemoved`).
    pub async fn cancel(&self, name: &str) -> Result<bool, RuntimeError> {
        self.cancel_with_timeout(name, self.cfg.grace).await
    }

    /// Cancel with explicit timeout.
    pub async fn cancel_with_timeout(
        &self,
        name: &str,
        wait_for: Duration,
    ) -> Result<bool, RuntimeError> {
        let exists_before = self.registry.contains(name).await;
        if !exists_before {
            return Ok(false);
        }

        let mut rx = self.bus.subscribe();
        self.bus.publish(
            Event::new(EventKind::TaskRemoveRequested)
                .with_task(name)
                .with_reason("manual_cancel"),
        );
        self.wait_task_removed(&mut rx, name, wait_for).await
    }

    /// Submits a task to the controller (if enabled).
    ///
    /// Returns an error if:
    /// - Controller feature is disabled
    /// - Controller is not configured
    /// - Submission queue is full (use `try_submit` for non-blocking)
    ///
    /// Requires the `controller` feature flag.
    #[cfg(feature = "controller")]
    pub async fn submit(
        &self,
        spec: crate::controller::ControllerSpec,
    ) -> Result<(), crate::controller::ControllerError> {
        match self.controller.get() {
            Some(ctrl) => ctrl.handle().submit(spec).await,
            None => Err(crate::controller::ControllerError::NotConfigured),
        }
    }

    /// Tries to submit a task without blocking.
    ///
    /// Returns `TrySubmitError::Full` if the queue is full.
    ///
    /// Requires the `controller` feature flag.
    #[cfg(feature = "controller")]
    pub fn try_submit(
        &self,
        spec: crate::controller::ControllerSpec,
    ) -> Result<(), crate::controller::ControllerError> {
        match self.controller.get() {
            Some(ctrl) => ctrl.handle().try_submit(spec),
            None => Err(crate::controller::ControllerError::NotConfigured),
        }
    }

    /// Listens to the internal event bus and fans out each received [`Event`] to all active subscribers.
    ///
    /// The listener also updates the [`AliveTracker`] before fan-out to keep
    /// the latest per-task state in sync.
    ///
    /// On `Lagged`, it creates a [`EventKind::SubscriberOverflow`] event and
    /// emits it **directly to subscribers**, bypassing the bus — to avoid
    /// reinforcing backpressure loops while still reporting the loss.
    ///
    /// The loop terminates only when the broadcast channel is closed.
    fn subscriber_listener(&self) {
        let mut rx = self.bus.subscribe();
        let set = Arc::clone(&self.subs);
        let alive = Arc::clone(&self.alive);

        tokio::spawn(async move {
            loop {
                match rx.recv().await {
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
                    Err(broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
        });
    }

    /// Waits for either shutdown signal or natural completion of all tasks.
    ///
    /// Both paths cancel `runtime_token` (stops registry/subscriber listeners)
    /// and drain subscriber queues via `subs.close()`.
    async fn drive_shutdown(&self) -> Result<(), RuntimeError> {
        let res = tokio::select! {
            _ = crate::core::shutdown::wait_for_shutdown_signal() => {
                self.bus.publish(Event::new(EventKind::ShutdownRequested));
                self.registry.cancel_all().await;
                self.wait_all_with_grace().await
            }
            _ = self.registry.wait_until_empty() => {
                Ok(())
            }
        };
        self.runtime_token.cancel();
        self.subs.close().await;
        res
    }

    /// Waits for all tasks in registry with grace period timeout.
    ///
    /// Publishes terminal event (`AllStoppedWithinGrace` or `GraceExceeded`).
    async fn wait_all_with_grace(&self) -> Result<(), RuntimeError> {
        let grace = self.cfg.grace;

        match timeout(grace, self.registry.wait_until_empty()).await {
            Ok(_) => {
                self.bus
                    .publish(Event::new(EventKind::AllStoppedWithinGrace));
                Ok(())
            }
            Err(_) => {
                self.bus.publish(Event::new(EventKind::GraceExceeded));
                let stuck = self.snapshot().await;
                Err(RuntimeError::GraceExceeded { grace, stuck })
            }
        }
    }

    async fn wait_task_removed(
        &self,
        rx: &mut broadcast::Receiver<Arc<Event>>,
        name: &str,
        wait_for: Duration,
    ) -> Result<bool, RuntimeError> {
        let target: Arc<str> = Arc::from(name);

        let wait_for_event = async {
            loop {
                match rx.recv().await {
                    Ok(ev)
                        if matches!(ev.kind, EventKind::TaskRemoved)
                            && ev.task.as_deref() == Some(&*target) =>
                    {
                        return Ok(true);
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if !self.registry.contains(&target).await {
                            return Ok(true);
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return Ok(!self.registry.contains(&target).await);
                    }
                }
            }
        };

        match timeout(wait_for, wait_for_event).await {
            Ok(result) => result,
            Err(_) => Err(RuntimeError::TaskRemoveTimeout {
                name: target,
                timeout: wait_for,
            }),
        }
    }
}
