//! # Supervisor: orchestrates task actors, fan-out delivery, and graceful shutdown.
//!
//! The [`Supervisor`] owns the event bus, a [`SubscriberSet`], and global runtime
//! configuration. It spawns per-task actors, handles OS signals, and enforces a
//! global concurrency cap via an optional semaphore.
//!
//! ## Key responsibilities
//! - subscribe to the [`Bus`] and **fan-out** events via [`SubscriberSet`]
//! - spawn task actors with restart/backoff/timeout policies
//! - handle OS termination signals (SIGINT/SIGTERM/Ctrl-C)
//! - perform graceful shutdown with a configurable [`Config::grace`]
//!
//! ## High-level architecture
//! ```text
//! Inputs to run():
//!   Vec<TaskSpec>  ──►  Supervisor::run(cfg, subscribers, bus)
//!
//! Preparation:
//!   - build_semaphore() from cfg.max_concurrent (None = unlimited)
//!   - subscriber_listener(): Bus.subscribe() ─► SubscriberSet::emit(&Event)   (fire-and-forget)
//!
//! Spawn actors:
//!   TaskSpec[0]  TaskSpec[1]  ...  TaskSpec[N-1]
//!       │            │                   │
//!       └──► TaskActor::new(task, params, bus, global_sem)        (one per spec)
//!                    └──► child CancellationToken = runtime_token.child_token()
//!                         set.spawn(actor.run(child_token))
//!
//! Event flow (as wired here):
//!   TaskActor ... ── publish(Event) ──► Bus ──► Supervisor listener ──► SubscriberSet::emit(&Event)
//!                                                                  ┌─────────┬─────────┐
//!                                                                  ▼         ▼         ▼
//!                                                           [queue S1] [queue S2] ... [queue SN]
//!                                                                  │         │         │
//!                                                           worker S1 worker S2 ... worker SN
//!                                                                  │         │         │
//!                                                         sub.on_event(&Event) (per subscriber)
//!
//! Shutdown path:
//!   shutdown::wait_for_shutdown_signal()
//!             └─► Bus.publish(ShutdownRequested)
//!             └─► runtime_token.cancel()   → propagates to child tokens
//!             └─► wait_all_with_grace(cfg.grace):
//!                    ├─ Ok (all joined)    → Bus.publish(AllStoppedWithinGrace)
//!                    └─ Timeout exceeded   → Bus.publish(GraceExceeded)
//!                                            (AliveTracker.snapshot() for stuck tasks)
//! ```
//!
//! - `Supervisor` spawns actors based on [`TaskSpec`].
//! - `SubscriberSet` fans out [`Event`]s to all subscribers without awaiting them.
//! - On OS signal, supervisor cancels all actors and waits up to [`Config::grace`].
//!
//! ## Example
//! ```rust
//! use std::sync::Arc;
//! use std::time::Duration;
//! use tokio_util::sync::CancellationToken;
//! use taskvisor::{
//!     BackoffPolicy, Config, RestartPolicy, Supervisor, TaskFn, TaskRef, TaskSpec,
//!     subscribers::{Subscribe, SubscriberSet},
//!     subscribers::predefined::AliveTracker,
//!     events::{Event, EventKind},
//! };
//! #[cfg(feature = "logging")]
//! use taskvisor::subscribers::predefined::LogWriter;
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let mut cfg = Config::default();
//!     cfg.max_concurrent = 2;
//!     cfg.grace = Duration::from_secs(5);
//!
//!     // Build subscribers:
//!     let alive = Arc::new(AliveTracker::new());
//!     let mut subs: Vec<Arc<dyn Subscribe>> = vec![alive.clone()];
//!     #[cfg(feature = "logging")]
//!     { subs.push(Arc::new(LogWriter::new())); }
//!
//!     let sup = Supervisor::new(cfg.clone(), subs, alive);
//!
//!     let t1: TaskRef = TaskFn::arc("ticker", |ctx: CancellationToken| async move {
//!         while !ctx.is_cancelled() {
//!             tokio::time::sleep(Duration::from_millis(250)).await;
//!         }
//!         Ok(())
//!     });
//!
//!     let spec = TaskSpec::new(
//!         t1,
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

use tokio::{sync::Semaphore, task::JoinSet};
use tokio_util::sync::CancellationToken;

use crate::core::{
    actor::{TaskActor, TaskActorParams},
    shutdown,
};
use crate::subscribers::{AliveTracker, Subscribe, SubscriberSet};
use crate::tasks::TaskSpec;
use crate::{
    config::Config,
    error::RuntimeError,
    events::Bus,
    events::{Event, EventKind},
};

/// Coordinates task actors, event delivery (via [`SubscriberSet`]), and graceful shutdown.
pub struct Supervisor {
    /// Global runtime configuration.
    pub cfg: Config,
    /// Event bus shared with all actors.
    pub bus: Bus,
    /// Fan-out set for subscribers.
    pub subs: Arc<SubscriberSet>,
    /// Handle to the alive-tracker used for final snapshot (same instance is in `subs`).
    pub alive: Arc<AliveTracker>,
}

impl Supervisor {
    /// Creates a new supervisor with the given config and the provided subscribers.
    ///
    /// `alive` **must** be the same instance as the one included into `subscribers`
    /// (and it will be added if absent).
    pub fn new(
        cfg: Config,
        mut subscribers: Vec<Arc<dyn Subscribe>>,
        alive: Arc<AliveTracker>,
    ) -> Self {
        let bus = Bus::new(cfg.bus_capacity);

        // Ensure `alive` is present in the set.
        let has_alive = subscribers
            .iter()
            .any(|s| std::ptr::eq::<dyn Subscribe>(&**s as _, &*alive as &dyn Subscribe));
        if !has_alive {
            subscribers.push(alive.clone());
        }

        let subs = Arc::new(SubscriberSet::new(subscribers));
        Self {
            cfg,
            bus,
            subs,
            alive,
        }
    }

    /// Runs the provided task specifications until either:
    /// - all actors exit on their own, or
    /// - a termination signal arrives → graceful shutdown (may end with `GraceExceeded`).
    pub async fn run(&self, tasks: Vec<TaskSpec>) -> Result<(), RuntimeError> {
        let semaphore = self.build_semaphore();
        let token = CancellationToken::new();
        self.subscriber_listener();

        let mut set = JoinSet::new();
        self.spawn_task_actors(&mut set, &token, &semaphore, tasks);
        self.drive_shutdown(&mut set, &token).await
    }

    /// Subscribes to the bus and forwards events to the subscriber set (fire-and-forget).
    fn subscriber_listener(&self) {
        let mut rx = self.bus.subscribe();
        let set = Arc::clone(&self.subs);
        tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                set.emit(&ev);
            }
        });
    }

    /// Builds a global semaphore if `max_concurrent > 0`; otherwise no cap.
    fn build_semaphore(&self) -> Option<Arc<Semaphore>> {
        match self.cfg.max_concurrent {
            0 => None,
            n => Some(Arc::new(Semaphore::new(n))),
        }
    }

    /// Spawns task actors and adds them to the given join set.
    fn spawn_task_actors(
        &self,
        set: &mut JoinSet<()>,
        runtime_token: &CancellationToken,
        global_sem: &Option<Arc<Semaphore>>,
        tasks: Vec<TaskSpec>,
    ) {
        for spec in tasks {
            let actor = TaskActor::new(
                spec.task.clone(),
                TaskActorParams {
                    restart: spec.restart,
                    backoff: spec.backoff,
                    timeout: spec.timeout,
                },
                self.bus.clone(),
                global_sem.clone(),
            );
            let child = runtime_token.child_token();
            set.spawn(actor.run(child));
        }
    }

    /// Waits until either all actors finish or a shutdown signal is received.
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

    /// Waits for all actors to finish within the configured grace period.
    ///
    /// Publishes [`EventKind::AllStoppedWithinGrace`] on success, or
    /// [`EventKind::GraceExceeded`] on timeout and returns
    /// [`RuntimeError::GraceExceeded`] with the list of stuck tasks.
    async fn wait_all_with_grace(&self, set: &mut JoinSet<()>) -> Result<(), RuntimeError> {
        let grace = self.cfg.grace;
        let done = async { while set.join_next().await.is_some() {} };
        let timed = tokio::time::timeout(grace, done).await;

        match timed {
            Ok(_) => {
                self.bus.publish(Event::now(EventKind::AllStoppedWithin));
                Ok(())
            }
            Err(_) => {
                self.bus.publish(Event::now(EventKind::GraceExceeded));
                let stuck = self.alive.snapshot(); // sync snapshot from RwLock
                Err(RuntimeError::GraceExceeded { grace, stuck })
            }
        }
    }
}
