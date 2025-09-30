//! # Supervisor: orchestrates task actors, lifecycle, and graceful shutdown.
//!
//! The [`Supervisor`] owns the event bus, an observer, and global runtime configuration.
//! It spawns per-task actors, handles OS signals, and enforces a global concurrency cap via an optional semaphore.
//!
//! Key responsibilities:
//! - subscribe and forward events to the [`Observer`]
//! - spawn task actors with restart/backoff/timeout policies
//! - handle OS termination signals (e.g. SIGINT/SIGTERM/Ctrl-C)
//! - perform graceful shutdown with a configurable [`Config::grace`]
//!
//! # High-level architecture:
//! ```text
//! Inputs to run():
//!   Vec<TaskSpec>  ──►  Supervisor::run(cfg, observer, bus)
//!
//! Preparation:
//!   - build_semaphore() from cfg.max_concurrent (None = unlimited)
//!   - observer_listener(): Bus.subscribe() ─► forward to Observer.on_event(&Event)
//!   - AliveTracker::new().spawn_listener(Bus.subscribe())
//!
//! Spawn actors:
//!   TaskSpec[0]  TaskSpec[1]  ...  TaskSpec[N-1]
//!       │            │                   │
//!       └──► TaskActor::new(task, params, bus, global_sem)   (one per spec)
//!                    └──► child CancellationToken = runtime_token.child_token()
//!                         set.spawn(actor.run(child_token))
//!
//! Event flow (as wired here):
//!   TaskActor ... ── publish(Event) ──► Bus ── broadcasts ──► Observer
//!                                                  └────────► AliveTracker
//!
//! Shutdown path:
//!   os_signals::wait_for_shutdown_signal()
//!             └─► Bus.publish(ShutdownRequested)
//!             └─► runtime_token.cancel()   → propagates to child tokens
//!             └─► wait_all_with_grace(cfg.grace):
//!                    ├─ Ok (all joined)    → Bus.publish(AllStoppedWithin)
//!                    └─ Timeout exceeded   → Bus.publish(GraceExceeded)
//!                                            (AliveTracker.snapshot() for stuck tasks)
//! ```
//!
//! - `Supervisor` spawns actors based on [`TaskSpec`].
//! - `Observer` subscribes to the [`Bus`] and processes [`Event`]s.
//! - On OS signal, supervisor cancels all actors and waits up to [`Config::grace`].
//!
//! # Example
//! ```
//! #![cfg(feature = "logging")]
//! use std::time::Duration;
//! use tokio_util::sync::CancellationToken;
//! use taskvisor::{
//!     BackoffPolicy,
//!     Config,
//!     RestartPolicy,
//!     Supervisor,
//!     TaskFn,
//!     TaskRef,
//!     TaskSpec,
//!     LogWriter,
//! };
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let mut cfg = Config::default();
//!     cfg.max_concurrent = 2;
//!     cfg.grace = Duration::from_secs(5);
//!
//!     let sup = Supervisor::new(cfg.clone(), LogWriter);
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

use crate::core::actor::{TaskActor, TaskActorParams};
use crate::core::shutdown;
use crate::observers::AliveTracker;
use crate::observers::Observer;
use crate::tasks::TaskSpec;
use crate::{
    config::Config,
    error::RuntimeError,
    events::Bus,
    events::{Event, EventKind},
};

/// # Coordinates task actors, event delivery, and graceful shutdown.
///
/// A `Supervisor`:
/// - forwards all bus events to the provided [`Observer`]
/// - spawns per-task actors using the given specs
/// - enforces a global concurrency limit (if [`Config::max_concurrent`] > 0)
/// - handles OS termination signals and waits up to [`Config::grace`] before failing
pub struct Supervisor<O: Observer + Send + Sync + 'static> {
    /// Global runtime configuration.
    pub cfg: Config,
    /// Observer used to process emitted runtime events.
    pub obs: Arc<O>,
    /// Event bus shared with all actors.
    pub bus: Bus,
}

impl<Obs: Observer + Send + Sync + 'static> Supervisor<Obs> {
    /// Creates a new supervisor with the given config and observer.
    pub fn new(cfg: Config, observer: Obs) -> Self {
        Self {
            bus: Bus::new(cfg.bus_capacity),
            obs: Arc::new(observer),
            cfg,
        }
    }

    /// Runs the provided task specifications until either:
    /// - all actors exit on their own (returns `Ok(())`), or
    /// - a termination signal is received (e.g. SIGINT/SIGTERM/Ctrl-C),
    ///   in which case graceful shutdown is initiated; if the grace period is exceeded, returns [`RuntimeError::GraceExceeded`].
    pub async fn run(&self, tasks: Vec<TaskSpec>) -> Result<(), RuntimeError> {
        let semaphore = self.build_semaphore();
        let token = CancellationToken::new();
        self.observer_listener();

        let alive = AliveTracker::new();
        alive.spawn_listener(self.bus.subscribe());

        let mut set = JoinSet::new();
        self.task_actors(&mut set, &token, &semaphore, tasks);
        self.shutdown(&mut set, &token, &alive).await
    }

    /// Subscribes the observer to the bus and forwards events asynchronously.
    fn observer_listener(&self) {
        let mut rx = self.bus.subscribe();
        let obs = self.obs.clone();

        tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                obs.on_event(&ev).await;
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

    /// Spawns task actors and registers them into the given join set.
    ///
    /// Each actor gets:
    /// - a child cancellation token (derived from the runtime token);
    /// - a bus clone;
    /// - an optional shared global semaphore.
    fn task_actors(
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

    /// Waits until either:
    /// - all actors finish naturally, or
    /// - a shutdown signal is received (SIGINT/SIGTERM/Ctrl-C).
    ///
    /// On shutdown, it publishes [`EventKind::ShutdownRequested`], cancels the
    /// runtime token, and then waits within the grace period ([`Supervisor::wait_all_with_grace`]).
    async fn shutdown(
        &self,
        set: &mut JoinSet<()>,
        runtime_token: &CancellationToken,
        alive: &AliveTracker,
    ) -> Result<(), RuntimeError> {
        tokio::select! {
            _ = shutdown::wait_for_shutdown_signal() => {
                self.bus.publish(Event::now(EventKind::ShutdownRequested));
                runtime_token.cancel();
                self.wait_all_with_grace(set, alive).await
            }
            _ = async { while set.join_next().await.is_some() {} } => {
                Ok(())
            }
        }
    }

    /// Waits for all actors to finish within the configured grace period.
    ///
    /// Publishes [`EventKind::AllStoppedWithin`] on success, or
    /// [`EventKind::GraceExceeded`] on timeout and returns
    /// [`RuntimeError::GraceExceeded`] with the list of stuck tasks.
    async fn wait_all_with_grace(
        &self,
        set: &mut JoinSet<()>,
        alive: &AliveTracker,
    ) -> Result<(), RuntimeError> {
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
                let stuck = alive.snapshot().await;
                Err(RuntimeError::GraceExceeded { grace, stuck })
            }
        }
    }
}
