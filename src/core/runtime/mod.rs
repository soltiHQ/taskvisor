//! Coordinates startup, management, events, queries, and shutdown for one supervisor.
//!
//! [`SupervisorBuilder`](crate::SupervisorBuilder) creates [`SupervisorCore`] from the registry,
//! event bus, subscribers, and cleanup ownership domain. Public [`Supervisor`](super::supervisor::Supervisor)
//! and [`SupervisorHandle`](super::handle::SupervisorHandle) methods then use this core to reach those components.
//!
//! ```text
//! Supervisor / SupervisorHandle
//!              ▼
//!       SupervisorCore
//!          ├── management ──► bounded command queue ────────► Registry
//!          ├── lifecycle ───► runtime workers and listeners
//!          ├── events ──────► Bus ──────────────────────────► event relay ──► subscriber queues
//!          └── shutdown ────────────────────────────────────► fence ────────► trigger-specific drain
//!                                                                                     ▼
//!                                                                                worker joins
//! ```
//!
//! The registry owns task membership and management decisions. Those decisions return through direct reply channels.
//! Events are best-effort and never drive runtime state. Activity queries also include force-aborted attempts that
//! remain active after registry membership ends. Requested and natural shutdown drain tasks.
//! Signal-setup failure skips that drain and still runs the common worker-cleanup tail.

mod event_relay;
mod lifecycle;
mod management;
#[cfg(feature = "controller")]
pub(crate) use management::ControllerAddPermit;
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
    SupervisorConfig, TaskDefaults, deferred_drop,
    registry::{Registry, RegistryCommand},
};
use crate::{events::Bus, subscribers::SubscriberSet};

/// Shares the internal runtime components and their ordering state.
///
/// Controller slot admission is a separate layer before this core.
pub(crate) struct SupervisorCore {
    /// Runtime configuration and task defaults fixed by the builder.
    settings: CoreSettings,
    /// Supervisor-local capacity and workers for deferred user-value destruction.
    drop_domain: deferred_drop::DropDomain,
    /// Best-effort event ingress for runtime components.
    pub(super) bus: Bus,
    /// Subscriber queues and callback workers.
    subs: Arc<SubscriberSet>,
    /// Authoritative task membership and actor owner.
    registry: Arc<Registry>,
    /// Stops runtime listeners during the common cleanup tail.
    runtime_token: CancellationToken,
    /// Records successful worker startup.
    started: AtomicBool,
    /// Serializes idempotent runtime startup.
    startup_gate: std::sync::Mutex<()>,
    /// Owns the single static-run lifecycle claim.
    running: AtomicBool,
    /// Records that shutdown closed management admission.
    shutting_down: AtomicBool,
    /// Shared shutdown operation and its cached outcome.
    shutdown: ShutdownCoordinator,
    /// Test-only ownership source used to inject bounded admission states.
    #[cfg(test)]
    ownership_source: std::sync::Mutex<Option<crate::core::deferred_drop::TestReservationSource>>,
    /// Optional controller whose workers follow this runtime lifecycle.
    #[cfg(feature = "controller")]
    controller: std::sync::OnceLock<std::sync::Weak<crate::controller::Controller>>,
    /// Orders the final command check against shutdown closure.
    admission_gate: std::sync::Mutex<()>,
    /// Sender for the bounded registry command queue.
    cmd_tx: mpsc::Sender<RegistryCommand>,
    /// Event relay handle when the bus is enabled.
    subscriber_handle: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Keeps builder-selected runtime settings together after construction.
pub(crate) struct CoreSettings {
    /// Runtime limits, queue sizes, and shutdown behavior.
    runtime: SupervisorConfig,
    /// Defaults applied when the registry accepts a task.
    task_defaults: TaskDefaults,
}

impl CoreSettings {
    /// Freezes both settings in one core-owned value.
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
    /// Builds shared coordination state from components wired by the builder.
    pub(crate) fn new_internal(
        settings: CoreSettings,
        bus: Bus,
        subs: Arc<SubscriberSet>,
        registry: Arc<Registry>,
        drop_domain: deferred_drop::DropDomain,
        runtime_token: CancellationToken,
        cmd_tx: mpsc::Sender<RegistryCommand>,
    ) -> Arc<Self> {
        Arc::new(Self {
            settings,
            drop_domain,
            bus,
            subs,
            registry,
            runtime_token,
            started: AtomicBool::new(false),
            startup_gate: std::sync::Mutex::new(()),
            running: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            shutdown: ShutdownCoordinator::new(),
            #[cfg(test)]
            ownership_source: std::sync::Mutex::new(None),
            #[cfg(feature = "controller")]
            controller: std::sync::OnceLock::new(),
            admission_gate: std::sync::Mutex::new(()),
            cmd_tx,
            subscriber_handle: std::sync::Mutex::new(None),
        })
    }

    /// Reports whether the command-admission fence has closed.
    pub(crate) fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }

    /// Exposes the builder-selected runtime configuration.
    pub(crate) fn runtime_config(&self) -> &SupervisorConfig {
        &self.settings.runtime
    }

    /// Exposes the defaults used at registry admission.
    pub(crate) fn task_defaults(&self) -> &TaskDefaults {
        &self.settings.task_defaults
    }

    /// Exposes this supervisor's cleanup ownership domain.
    pub(crate) fn drop_domain(&self) -> &deferred_drop::DropDomain {
        &self.drop_domain
    }

    /// Returns the reliable shutdown-start signal used by the controller.
    #[cfg(feature = "controller")]
    pub(crate) fn shutdown_started_token(&self) -> CancellationToken {
        self.shutdown.started.clone()
    }

    /// Adds one controller to this core's startup and shutdown lifecycle.
    ///
    /// # Panics
    ///
    /// Panics if a controller was already attached.
    #[cfg(feature = "controller")]
    pub(crate) fn attach_controller(&self, controller: &Arc<crate::controller::Controller>) {
        assert!(
            self.controller.set(Arc::downgrade(controller)).is_ok(),
            "the controller lifecycle may be attached only once"
        );
    }
}
