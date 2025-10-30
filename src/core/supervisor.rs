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
//! Supervisor::run()
//!     ├──► subscriber_listener()
//!     │     ├──► updates AliveTracker
//!     │     └──► fans out to SubscriberSet
//!     ├──► Registry.spawn_listener()
//!     │     ├──► TaskAddRequested → spawn actor
//!     │     ├──► TaskRemoveRequested → cancel task
//!     │     ├──► ActorExhausted → cleanup
//!     │     └──► ActorDead → cleanup
//!     └──► drive_shutdown()
//!           ├──► wait for signal or empty registry
//!           ├──► cancel all tasks
//!           └──► wait_all_with_grace
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
//! use taskvisor::{Config, Supervisor, TaskSpec, TaskFn, RestartPolicy, BackoffPolicy};
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let cfg = Config::default();
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

use std::{sync::Arc, time::Duration};
use tokio::{sync::Notify, sync::OnceCell, sync::broadcast, time::timeout};
use tokio_util::sync::CancellationToken;

use crate::core::{alive::AliveTracker, builder::SupervisorBuilder, registry::Registry};
use crate::{
    config::Config,
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
    cfg: Config,
    bus: Bus,
    subs: Arc<SubscriberSet>,
    alive: Arc<AliveTracker>,
    registry: Arc<Registry>,
    ready: Arc<Notify>,
    runtime_token: CancellationToken,

    #[cfg(feature = "controller")]
    pub(super) controller: OnceCell<Arc<crate::controller::Controller>>,
}

impl Supervisor {
    /// Internal constructor used by builder (not public API).
    pub(crate) fn new_internal(
        cfg: Config,
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

            #[cfg(feature = "controller")]
            controller: OnceCell::new(),
        }
    }

    /// Creates a new supervisor with the given config and subscribers (maybe empty).
    pub fn new(cfg: Config, subscribers: Vec<Arc<dyn Subscribe>>) -> Arc<Self> {
        Self::builder(cfg).with_subscribers(subscribers).build()
    }

    /// Creates a builder for constructing a Supervisor.
    ///
    /// ## Example
    /// ```rust
    /// use taskvisor::{Config, Supervisor};
    ///
    /// let sup = Supervisor::builder(Config::default())
    ///     .with_subscribers(vec![])
    ///     .build();
    /// ```
    pub fn builder(cfg: Config) -> SupervisorBuilder {
        SupervisorBuilder::new(cfg)
    }

    /// Waits until supervisor is fully initialized and ready to accept submissions.
    pub async fn wait_ready(&self) {
        self.ready.notified().await;
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
    pub async fn list_tasks(&self) -> Vec<String> {
        self.registry.list().await
    }

    /// Runs task specifications until completion or shutdown signal.
    ///
    /// Steps:
    /// - Spawn subscriber listener (event fan-out)
    /// - Spawn registry listener (task lifecycle management)
    /// - Notify waiters: ready for submit jobs
    /// - Publish TaskAddRequested for initial tasks
    /// - Optionally wait until registry becomes non-empty (if we added tasks)
    /// - Wait for shutdown signal or all tasks to exit
    pub async fn run(&self, tasks: Vec<TaskSpec>) -> Result<(), RuntimeError> {
        self.subscriber_listener();
        self.registry.clone().spawn_listener();
        self.ready.notify_waiters();

        let expected = tasks.len();
        for spec in tasks {
            self.add_task(spec)?;
        }
        if expected > 0 {
            self.registry.wait_became_nonempty_once().await;
        }
        self.drive_shutdown().await
    }

    /// Returns sorted list of currently alive task names.
    pub async fn snapshot(&self) -> Vec<String> {
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
        let exists_before = {
            let tasks = self.registry.list().await;
            tasks.contains(&name.to_string())
        };
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
    async fn drive_shutdown(&self) -> Result<(), RuntimeError> {
        tokio::select! {
            _ = crate::core::shutdown::wait_for_shutdown_signal() => {
                self.bus.publish(Event::new(EventKind::ShutdownRequested));
                self.registry.cancel_all().await;
                let res = self.wait_all_with_grace().await;
                self.runtime_token.cancel();
                res
            }
            _ = self.registry.wait_until_empty() => { Ok(()) }
        }
    }

    /// Waits for all tasks in registry with grace period timeout.
    ///
    /// Publishes terminal event (`AllStoppedWithinGrace` or `GraceExceeded`).
    async fn wait_all_with_grace(&self) -> Result<(), RuntimeError> {
        use tokio::time::{Duration, sleep};

        let grace = self.cfg.grace;
        let done = async {
            loop {
                if self.registry.is_empty().await {
                    return;
                }
                sleep(Duration::from_millis(100)).await;
            }
        };

        match timeout(grace, done).await {
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
        let target = name.to_string();
        let start = tokio::time::Instant::now();
        let mut last_poll = tokio::time::Instant::now();
        let poll_interval = Duration::from_millis(100);

        loop {
            if start.elapsed() >= wait_for {
                return Err(RuntimeError::TaskRemoveTimeout {
                    name: target,
                    timeout: wait_for,
                });
            }
            if last_poll.elapsed() >= poll_interval {
                let tasks = self.registry.list().await;
                if !tasks.contains(&target) {
                    return Ok(true);
                }
                last_poll = tokio::time::Instant::now();
            }

            let recv_timeout = poll_interval
                .checked_sub(last_poll.elapsed())
                .unwrap_or(Duration::from_millis(10));

            match tokio::time::timeout(recv_timeout, rx.recv()).await {
                Ok(Ok(ev))
                    if matches!(ev.kind, EventKind::TaskRemoved)
                        && ev.task.as_deref() == Some(&target) =>
                {
                    return Ok(true);
                }
                Ok(Ok(_)) => {}
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    let tasks = self.registry.list().await;
                    return Ok(!tasks.contains(&target));
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                    let tasks = self.registry.list().await;
                    if !tasks.contains(&target) {
                        return Ok(true);
                    }
                    last_poll = tokio::time::Instant::now();
                }
                Err(_elapsed) => {}
            }
        }
    }
}
