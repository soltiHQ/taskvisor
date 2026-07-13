//! # Runtime coordinator
//!
//! [`SupervisorCore`] connects the registry, event bus, subscribers, alive
//! tracker, and shutdown state behind the public
//! [`Supervisor`](super::supervisor::Supervisor).
//!
//! ## Data Paths
//!
//! | Path | Route |
//! |------|-------|
//! | Management | handle -> core -> bounded registry queue -> registry -> direct reply |
//! | Events | runtime components -> event bus -> event relay -> alive tracker and subscriber queues |
//! | Shared shutdown | winning trigger -> shared owner -> admission fence -> task drain when applicable -> common cleanup |
//! | Last-owner fallback | close management admission -> cancel shutdown and runtime tokens; no shared result |
//!
//! Management commands use direct replies.
//! They do not depend on the best-effort event bus.
//! Alive snapshots do use the event bus and may lag.
//!
//! ## Modes
//!
//! | Entry point  | Behavior                                                                                                                                 |
//! |--------------|------------------------------------------------------------------------------------------------------------------------------------------|
//! | `start()`    | Start subscriber workers, the event relay, and the registry listener once                                                                |
//! | `run(tasks)` | Start the runtime, register a non-empty task set as one atomic batch, then wait for shared shutdown, an OS signal, or registry emptiness |
//! | `shutdown()` | Start or join shared cleanup and return its cached result                                                                                |
//!
//! ## Rules
//!
//! - Management command admission closes once shutdown begins.
//! - Shutdown processes commands committed before the admission gate closed.
//! - Explicit, signal, and natural shutdown share one cleanup operation.
//! - Every shutdown caller receives the same cached result.
//! - The first trigger that installs the shared shutdown operation selects its result.
//!   `ShutdownRequested` is emitted only when an explicit request or received OS signal wins that race.
//! - Static `run()` tasks are accepted or rejected as one batch.
//! - Registry membership is keyed by `TaskId`.
//! - `run()` is single-shot. A second call returns `RuntimeError::AlreadyRunning`.
//! - `snapshot` and `is_alive` are best-effort event-based views.
//! - `start()` is idempotent.

mod event_relay;
mod lifecycle;
mod management;
mod shutdown_workflow;

#[cfg(test)]
mod tests;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use self::shutdown_workflow::ShutdownCoordinator;
use crate::core::{
    SupervisorConfig, TaskDefaults,
    alive::AliveTracker,
    registry::{Registry, RegistryCommand},
};
use crate::{events::Bus, subscribers::SubscriberSet};

/// Runtime coordinator behind [`Supervisor`](super::supervisor::Supervisor).
///
/// Controller slot admission remains outside this core.
pub(crate) struct SupervisorCore {
    settings: CoreSettings,
    pub(super) bus: Bus,
    subs: Arc<SubscriberSet>,
    alive: Arc<AliveTracker>,
    registry: Arc<Registry>,
    runtime_token: CancellationToken,
    started: AtomicBool,
    startup_gate: std::sync::Mutex<()>,
    running: AtomicBool,
    shutting_down: AtomicBool,
    shutdown: ShutdownCoordinator,
    #[cfg(feature = "controller")]
    controller: std::sync::OnceLock<std::sync::Weak<crate::controller::Controller>>,
    admission_gate: std::sync::Mutex<()>,
    cmd_tx: mpsc::Sender<RegistryCommand>,
    subscriber_handle: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Immutable configuration bundle shared by runtime components.
pub(crate) struct CoreSettings {
    runtime: SupervisorConfig,
    task_defaults: TaskDefaults,
}

impl CoreSettings {
    pub(crate) fn new(runtime: SupervisorConfig, task_defaults: TaskDefaults) -> Self {
        Self {
            runtime,
            task_defaults,
        }
    }
}

impl std::fmt::Debug for SupervisorCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupervisorCore")
            .field("runtime", &self.settings.runtime)
            .field("task_defaults", &self.settings.task_defaults)
            .field("started", &self.started.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl SupervisorCore {
    /// Creates a ready-to-share runtime core.
    ///
    /// Used by the builder after all runtime components have been wired.
    pub(crate) fn new_internal(
        settings: CoreSettings,
        bus: Bus,
        subs: Arc<SubscriberSet>,
        alive: Arc<AliveTracker>,
        registry: Arc<Registry>,
        runtime_token: CancellationToken,
        cmd_tx: mpsc::Sender<RegistryCommand>,
    ) -> Arc<Self> {
        Arc::new(Self {
            settings,
            bus,
            subs,
            alive,
            registry,
            runtime_token,
            started: AtomicBool::new(false),
            startup_gate: std::sync::Mutex::new(()),
            running: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            shutdown: ShutdownCoordinator::new(),
            #[cfg(feature = "controller")]
            controller: std::sync::OnceLock::new(),
            admission_gate: std::sync::Mutex::new(()),
            cmd_tx,
            subscriber_handle: std::sync::Mutex::new(None),
        })
    }

    /// Returns true once shutdown has started and management admission is closed.
    pub(crate) fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }

    /// Returns immutable runtime settings.
    pub(crate) fn runtime_config(&self) -> &SupervisorConfig {
        &self.settings.runtime
    }

    /// Returns immutable task defaults used at registry admission.
    pub(crate) fn task_defaults(&self) -> &TaskDefaults {
        &self.settings.task_defaults
    }

    /// Returns the reliable signal fired when shutdown starts, including the last-owner fallback.
    #[cfg(feature = "controller")]
    pub(crate) fn shutdown_started_token(&self) -> CancellationToken {
        self.shutdown.started.clone()
    }

    /// Registers the optional controller as a participant in shared runtime cleanup.
    #[cfg(feature = "controller")]
    pub(crate) fn attach_controller(&self, controller: &Arc<crate::controller::Controller>) {
        assert!(
            self.controller.set(Arc::downgrade(controller)).is_ok(),
            "the controller lifecycle may be attached only once"
        );
    }
}
