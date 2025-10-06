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
//!     │
//!     ├──► subscriber_listener()
//!     │         ├──► updates AliveTracker
//!     │         └──► fans out to SubscriberSet
//!     │
//!     ├──► Registry.spawn_listener()
//!     │         ├──► TaskAddRequested → spawn actor
//!     │         ├──► TaskRemoveRequested → cancel task
//!     │         ├──► ActorExhausted → cleanup
//!     │         └──► ActorDead → cleanup
//!     │
//!     └──► drive_shutdown()
//!           ├──► wait for signal or empty registry
//!           ├──► cancel all tasks
//!           └──► wait_all_with_grace
//! ```
//!
//! ## Runtime task management
//! ```text
//! add_task(spec)
//!     │
//!     ├──► Bus.publish(TaskAddRequested + spec)
//!     │
//!     └──► Registry.event_listener
//!           ├──► spawn actor
//!           ├──► registry.insert(handle)
//!           └──► Bus.publish(TaskAdded)
//!
//! remove_task(name)
//!     │
//!     ├──► Bus.publish(TaskRemoveRequested + name)
//!     │
//!     └──► Registry.event_listener
//!           ├──► registry.remove(name) + cancel token
//!           ├──► await actor finish
//!           └──► Bus.publish(TaskRemoved)
//!
//! Actor finishes
//!     │
//!     ├──► Bus.publish(ActorExhausted/ActorDead)
//!     │
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
//! use taskvisor::{Config, Supervisor, TaskSpec, TaskFn, RestartPolicy, BackoffPolicy};
//! use tokio_util::sync::CancellationToken;
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

use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::core::{alive::AliveTracker, registry::Registry, shutdown};
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
    /// Global runtime configuration.
    cfg: Config,
    /// Event bus shared with all actors.
    bus: Bus,
    /// Fan-out set for subscribers.
    subs: Arc<SubscriberSet>,
    /// Tracker of alive tasks for stuck detection.
    alive: Arc<AliveTracker>,
    /// Task registry for managing active tasks.
    registry: Arc<Registry>,
    /// Runtime cancellation token (shared with registry).
    runtime_token: CancellationToken,
}

impl Supervisor {
    /// Creates a new supervisor with the given config and subscribers (maybe empty).
    pub fn new(cfg: Config, subscribers: Vec<Arc<dyn Subscribe>>) -> Self {
        let bus = Bus::new(cfg.bus_capacity);
        let subs = Arc::new(SubscriberSet::new(subscribers, bus.clone()));
        let runtime_token = CancellationToken::new();
        let semaphore = Self::build_semaphore_static(&cfg);

        let registry = Registry::new(bus.clone(), runtime_token.clone(), semaphore);

        Self {
            cfg,
            bus,
            subs,
            alive: Arc::new(AliveTracker::new()),
            registry,
            runtime_token,
        }
    }

    /// Adds a new task to the supervisor at runtime.
    ///
    /// Publishes `TaskAddRequested` event with spec to the bus.
    /// Registry listener will spawn the actor.
    ///
    /// ### Example
    /// ```rust,ignore
    /// let spec = TaskSpec::new(task, RestartPolicy::Always, BackoffPolicy::default(), None);
    /// supervisor.add_task(spec).await?;
    /// ```
    pub fn add_task(&self, spec: TaskSpec) -> Result<(), RuntimeError> {
        self.bus.publish(
            Event::now(EventKind::TaskAddRequested)
                .with_task(spec.task().name())
                .with_spec(spec),
        );
        Ok(())
    }

    /// Removes a task from the supervisor at runtime.
    ///
    /// Publishes `TaskRemoveRequested` event to the bus.
    /// Registry listener will cancel and remove the task.
    ///
    /// ### Example
    /// ```rust,ignore
    /// supervisor.remove_task("worker-1").await?;
    /// ```
    pub fn remove_task(&self, name: &str) -> Result<(), RuntimeError> {
        self.bus
            .publish(Event::now(EventKind::TaskRemoveRequested).with_task(name));
        Ok(())
    }

    /// Returns a sorted list of currently active task names from the registry.
    ///
    /// ### Example
    /// ```rust,ignore
    /// let tasks = supervisor.list_tasks().await;
    /// println!("Active tasks: {:?}", tasks);
    /// ```
    pub async fn list_tasks(&self) -> Vec<String> {
        self.registry.list().await
    }

    /// Runs task specifications until completion or shutdown signal.
    ///
    /// ### Flow
    /// 1. Spawn subscriber listener (event fan-out)
    /// 2. Spawn registry listener (task lifecycle management)
    /// 3. Publish TaskAddRequested for initial tasks
    /// 4. Wait for shutdown signal or all tasks to exit
    /// 5. Perform graceful shutdown with grace period
    ///
    /// ### Exit conditions
    /// - **All actors exit naturally** → returns `Ok(())`
    /// - **OS signal received** → graceful shutdown:
    ///   - Cancels `runtime_token` (propagates to all actors)
    ///   - Waits up to `Config::grace` for actors to finish
    ///   - Returns `Ok(())` if all stopped within grace
    ///   - Returns `Err(GraceExceeded)` with stuck task names otherwise
    pub async fn run(&self, tasks: Vec<TaskSpec>) -> Result<(), RuntimeError> {
        self.subscriber_listener();
        self.registry.clone().spawn_listener();

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
    ///
    /// Used internally during shutdown to detect stuck tasks.
    /// May also be useful for external monitoring/debugging.
    pub async fn snapshot(&self) -> Vec<String> {
        self.alive.snapshot().await
    }

    /// Check and return boolean task status (alive / not alive).
    pub async fn is_alive(&self, name: &str) -> bool {
        self.alive.is_alive(name).await
    }

    /// Spawns background task that:
    /// 1. Subscribes to event bus
    /// 2. Updates alive tracker (with sequence-based ordering)
    /// 3. Fans out events to subscribers
    ///
    /// ### Rules
    /// - Runs until bus is closed (when Supervisor is dropped)
    /// - Handles `Lagged` errors gracefully (skips old events)
    fn subscriber_listener(&self) {
        let mut rx = self.bus.subscribe();
        let set = Arc::clone(&self.subs);
        let alive = Arc::clone(&self.alive);
        let rt = self.runtime_token.clone();
        let bus = self.bus.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = rt.cancelled() => break,
                    msg = rx.recv() => match msg {
                        Ok(ev) => {
                            let arc_ev = Arc::new(ev);
                            alive.update(&arc_ev).await;
                            set.emit_arc(arc_ev);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            bus.publish(
                                Event::now(EventKind::TaskFailed)
                                    .with_error("subscriber_listener_lagged")
                            );
                            continue;
                        }
                    }
                }
            }
        });
    }

    /// Builds global semaphore for concurrency limiting.
    ///
    /// Returns `None` if `max_concurrent == 0` (unlimited).
    fn build_semaphore_static(cfg: &Config) -> Option<Arc<Semaphore>> {
        match cfg.max_concurrent {
            0 => None,
            n => Some(Arc::new(Semaphore::new(n))),
        }
    }

    /// Waits for either shutdown signal or natural completion of all tasks.
    async fn drive_shutdown(&self) -> Result<(), RuntimeError> {
        tokio::select! {
            _ = crate::core::shutdown::wait_for_shutdown_signal() => {
                self.bus.publish(Event::now(EventKind::ShutdownRequested));
                self.runtime_token.cancel();
                self.registry.cancel_all().await;
                self.wait_all_with_grace().await
            }
            _ = self.registry.wait_until_empty() => {
                Ok(())
            }
        }
    }

    /// Waits until registry becomes empty (all tasks finished naturally).
    async fn wait_for_empty_registry(&self) {
        use tokio::time::{Duration, sleep};
        loop {
            if self.registry.is_empty().await {
                return;
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    /// Waits for all tasks in registry with grace period timeout.
    ///
    /// Publishes terminal event (`AllStoppedWithin` or `GraceExceeded`).
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
                self.bus.publish(Event::now(EventKind::AllStoppedWithin));
                Ok(())
            }
            Err(_) => {
                self.bus.publish(Event::now(EventKind::GraceExceeded));
                let stuck = self.snapshot().await;
                Err(RuntimeError::GraceExceeded { grace, stuck })
            }
        }
    }
}
