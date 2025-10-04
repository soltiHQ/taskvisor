//! # Supervisor: orchestrates task actors and graceful shutdown.
//!
//! The [`Supervisor`] owns the runtime components (event bus, subscribers, alive tracker)
//! and orchestrates task execution lifecycle from spawn to graceful termination.
//!
//! - Spawn task actors with execution policies from [`TaskSpec`]
//! - Perform graceful shutdown with configurable grace period
//! - Enforce global concurrency limits via optional semaphore
//! - Track alive tasks for stuck detection during shutdown
//! - Fan-out events to subscribers via [`SubscriberSet`]
//! - Handle OS termination signals
//!
//! ## Architecture
//! ```text
//! TaskSpec[] ──► Supervisor::run()
//!                     │
//!                     ├──► spawn TaskActor per spec
//!                     │         └──► publishes events to Bus
//!                     │
//!                     ├──► subscriber_listener()
//!                     │         ├──► updates AliveTracker
//!                     │         └──► fans out to SubscriberSet
//!                     │
//!                     └──► wait for:
//!                           ├──► all actors exit (Ok)
//!                           └──► OS signal → graceful shutdown
//!                                 ├──► cancel all actors
//!                                 ├──► wait up to grace period
//!                                 └──► check stuck tasks (AliveTracker)
//! ```
//!
//! ## Rules
//! - Alive tracking uses **sequence numbers** (handles out-of-order events)
//! - Subscriber fan-out is **non-blocking** (per-subscriber queues)
//! - Graceful shutdown waits **at most** `Config::grace` duration
//! - Stuck tasks are reported via `RuntimeError::GraceExceeded`
//! - Global concurrency limit applies across **all** actors
//!
//! ```rust
//! use std::time::Duration;
//! use taskvisor::{Config, Supervisor, TaskSpec, TaskFn, RestartPolicy, BackoffPolicy};
//! use tokio_util::sync::CancellationToken;
//!
//! #[cfg(feature = "logging")]
//! use taskvisor::LogWriter;
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let mut cfg = Config::default();
//!     cfg.max_concurrent = 2;
//!     cfg.grace = Duration::from_secs(5);
//!
//!     let sup = Supervisor::new(cfg, { Vec::new() });
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

use tokio::{sync::Semaphore, task::JoinSet, time::timeout};
use tokio_util::sync::CancellationToken;

use crate::core::{
    actor::{TaskActor, TaskActorParams},
    alive::AliveTracker,
    shutdown,
};
use crate::{
    config::Config,
    error::RuntimeError,
    events::{Bus, Event, EventKind},
    subscribers::{Subscribe, SubscriberSet},
    tasks::TaskSpec,
};

/// Orchestrates task actors, event delivery, and graceful shutdown.
///
/// - Spawns and supervises task actors based on [`TaskSpec`]
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
}

impl Supervisor {
    /// Creates a new supervisor with the given config and subscribers (maybe empty).
    pub fn new(cfg: Config, subscribers: Vec<Arc<dyn Subscribe>>) -> Self {
        let bus = Bus::new(cfg.bus_capacity);
        let subs = Arc::new(SubscriberSet::new(subscribers, bus.clone()));

        Self {
            cfg,
            bus,
            subs,
            alive: Arc::new(AliveTracker::new()),
        }
    }

    /// Runs task specifications until completion or shutdown signal.
    ///
    /// ### Exit conditions
    /// - **All actors exit naturally** → returns `Ok(())`
    /// - **OS signal received** → graceful shutdown:
    ///   - Cancels all actors (via `runtime_token`)
    ///   - Waits up to `Config::grace` for actors to finish
    ///   - Returns `Ok(())` if all stopped within grace
    ///   - Returns `Err(GraceExceeded)` with stuck task names otherwise
    ///
    /// ### Graceful shutdown flow
    /// - Receive OS signal
    /// - Publish `ShutdownRequested` event
    /// - Cancel `runtime_token` (propagates to all actors)
    /// - Wait up to `Config::grace` for actors to finish
    /// - Check alive tracker for stuck tasks
    /// - Return result
    pub async fn run(&self, tasks: Vec<TaskSpec>) -> Result<(), RuntimeError> {
        let semaphore = self.build_semaphore();
        let runtime_token = CancellationToken::new();

        // Spawn listener before actors to avoid missing early events
        self.subscriber_listener();

        let mut set = JoinSet::new();
        self.spawn_task_actors(&mut set, &runtime_token, &semaphore, tasks);
        self.drive_shutdown(&mut set, &runtime_token).await
    }

    /// Returns sorted list of currently alive task names.
    ///
    /// Used internally during shutdown to detect stuck tasks.
    /// May also be useful for external monitoring/debugging.
    pub async fn snapshot(&self) -> Vec<String> {
        self.alive.snapshot().await
    }

    // Check and return boolean task status (alive / not alive).
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

        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        let arc_ev = Arc::new(ev);
                        alive.update(&arc_ev).await;
                        set.emit_arc(arc_ev);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });
    }

    /// Builds global semaphore for concurrency limiting.
    ///
    /// Returns `None` if `max_concurrent == 0` (unlimited).
    fn build_semaphore(&self) -> Option<Arc<Semaphore>> {
        match self.cfg.max_concurrent {
            0 => None,
            n => Some(Arc::new(Semaphore::new(n))),
        }
    }

    /// Spawns one actor per task spec.
    fn spawn_task_actors(
        &self,
        set: &mut JoinSet<()>,
        runtime_token: &CancellationToken,
        semaphore: &Option<Arc<Semaphore>>,
        tasks: Vec<TaskSpec>,
    ) {
        for spec in tasks {
            let actor = TaskActor::new(
                self.bus.clone(),
                spec.task().clone(),
                TaskActorParams {
                    restart: spec.restart(),
                    backoff: spec.backoff(),
                    timeout: spec.timeout(),
                },
                semaphore.clone(),
            );
            let child = runtime_token.child_token();
            set.spawn(actor.run(child));
        }
    }

    /// Waits for either natural completion or shutdown signal.
    async fn drive_shutdown(
        &self,
        set: &mut JoinSet<()>,
        runtime_token: &CancellationToken,
    ) -> Result<(), RuntimeError> {
        tokio::select! {
            _ = shutdown::wait_for_shutdown_signal() => {
                self.bus.publish(Event::now(EventKind::ShutdownRequested));
                runtime_token.cancel();
                self.wait_all_with_grace(set).await
            }
            _ = async { while set.join_next().await.is_some() {} } => {
                Ok(())
            }
        }
    }

    /// Waits for all actors with grace period timeout.
    ///
    /// Publishes terminal event (`AllStoppedWithin` or `GraceExceeded`).
    async fn wait_all_with_grace(&self, set: &mut JoinSet<()>) -> Result<(), RuntimeError> {
        let grace = self.cfg.grace;
        let done = async { while set.join_next().await.is_some() {} };
        let timed = timeout(grace, done).await;

        match timed {
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
