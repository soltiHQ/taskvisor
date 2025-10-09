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

use std::sync::Arc;

use tokio::{sync::Semaphore, sync::broadcast, time::timeout};
use tokio_util::sync::CancellationToken;

use crate::core::{alive::AliveTracker, registry::Registry};
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
    runtime_token: CancellationToken,
}

impl Supervisor {
    /// Creates a new supervisor with the given config and subscribers (maybe empty).
    pub fn new(cfg: Config, subscribers: Vec<Arc<dyn Subscribe>>) -> Self {
        let bus = Bus::new(cfg.bus_capacity_clamped());
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
    /// 1) Spawn subscriber listener (event fan-out)
    /// 2) Spawn registry listener (task lifecycle management)
    /// 3) Publish TaskAddRequested for initial tasks
    /// 4) Optionally wait until registry becomes non-empty (if we added tasks)
    /// 5) Wait for shutdown signal or all tasks to exit
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
    pub async fn snapshot(&self) -> Vec<String> {
        self.alive.snapshot().await
    }

    /// Check whether a given task is currently alive.
    pub async fn is_alive(&self, name: &str) -> bool {
        self.alive.is_alive(name).await
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
                    Ok(ev) => {
                        let arc_ev = Arc::new(ev);
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

    /// Builds global semaphore for concurrency limiting.
    ///
    /// Returns `None` if unlimited.
    fn build_semaphore_static(cfg: &Config) -> Option<Arc<Semaphore>> {
        cfg.concurrency_limit().map(Semaphore::new).map(Arc::new)
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
                self.bus.publish(Event::new(EventKind::AllStoppedWithin));
                Ok(())
            }
            Err(_) => {
                self.bus.publish(Event::new(EventKind::GraceExceeded));
                let stuck = self.snapshot().await;
                Err(RuntimeError::GraceExceeded { grace, stuck })
            }
        }
    }
}
