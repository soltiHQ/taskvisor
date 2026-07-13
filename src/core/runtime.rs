//! # Supervisor runtime core.
//!
//! [`SupervisorCore`] owns the runtime components behind the public [`Supervisor`](super::supervisor::Supervisor) facade.
//!
//! It wires together:
//! - registry command channel,
//! - [`Registry`],
//! - event bus,
//! - subscriber fan-out,
//! - alive-task tracker,
//! - runtime shutdown token.
//!
//! ## Planes
//!
//! ```text
//! Management plane:
//!   SupervisorHandle -> SupervisorCore -> mpsc -> Registry
//!
//! Event plane:
//!   runtime components -> Bus -> subscriber_listener
//!                               -> AliveTracker
//!                               -> SubscriberSet
//!
//! Shutdown plane:
//!   OS signal / handle.shutdown -> drain_with_grace
//!                               -> cancel registry tasks
//!                               -> join listeners
//!                               -> close subscribers
//! ```
//!
//! Add, remove, and cancel commands use the management plane.
//! They are not delivered through the lossy event bus.
//!
//! Events are used for observability, alive snapshots, and subscriber delivery.
//! Event consumers may lag.
//!
//! ## Modes
//!
//! ```text
//! start()
//!   starts subscriber listener and registry listener
//!
//! run(tasks)
//!   starts listeners
//!   registers all initial tasks as one atomic batch
//!   waits for the direct registry reply
//!   waits for OS shutdown signal or natural completion
//!
//! shutdown()
//!   cancels all tasks
//!   waits up to grace
//!   force-aborts tasks that do not stop
//!   joins internal listeners
//! ```
//!
//! ## Rules
//!
//! - New task admission closes once shutdown begins.
//! - Shutdown waits for a registry fence before it starts task drain.
//! - Commands committed before the admission gate closes are processed before that fence.
//! - Explicit, signal, and natural shutdown paths join one detached operation.
//! - Every shutdown waiter receives the same cached result after full cleanup.
//! - The first shutdown trigger controls the result and request events.
//! - Static `run()` tasks are accepted or rejected as one registry operation.
//! - Registry membership is keyed by `TaskId`.
//! - `run()` is single-shot. A second call returns `RuntimeError::AlreadyRunning`.
//! - `snapshot` and `is_alive` are best-effort views from the alive tracker.
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

/// Runtime implementation behind the public [`Supervisor`](super::supervisor::Supervisor).
///
/// This type is the composition root for the management, event, lifecycle, and
/// shutdown planes. Controller admission remains outside the runtime core.
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

    /// Returns the reliable signal that fires when the shared shutdown operation starts.
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
