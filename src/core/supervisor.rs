//! # Public supervisor facade.
//!
//! [`Supervisor`] is the public entry point for running taskvisor.
//!
//! It owns the runtime core and, when the `controller` feature is enabled, an optional controller.
//! The runtime implementation lives in [`SupervisorCore`]; this type keeps the public API small and stable.
//!
//! ## Modes
//!
//! - [`run`](Supervisor::run): run a fixed set of tasks until natural completion or an OS shutdown signal.
//! - [`serve`](Supervisor::serve): start the runtime and return a [`SupervisorHandle`](crate::SupervisorHandle) for dynamic task management.
//!
//! ## Ownership and Drop
//!
//! `Supervisor` and every [`SupervisorHandle`](crate::SupervisorHandle) share one
//! public-owner lease. Dropping an individual clone has no runtime effect. When
//! the final public owner is dropped, taskvisor closes admission and sends
//! best-effort cancellation without blocking. Use `shutdown().await` when the
//! caller needs graceful cleanup, joins, and an explicit result.
//!
//! [`SupervisorCore`]: crate::core::SupervisorCore

use std::sync::Arc;

use crate::core::{RuntimeOwner, SupervisorConfig, SupervisorCore, builder::SupervisorBuilder};
use crate::{error::RuntimeError, subscribers::Subscribe, tasks::TaskSpec};

/// Public facade for the taskvisor runtime.
///
/// Use [`new`](Self::new) for simple construction, or [`builder`](Self::builder)
/// when you need custom task defaults or optional features.
///
/// ## Static Mode
///
/// Static mode runs a known task set:
///
/// ```rust,no_run
/// # use taskvisor::prelude::*;
/// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
/// supervisor.run(vec![/* task specs */]).await?;
/// # Ok(()) }
/// ```
///
/// ## Dynamic Mode
///
/// Dynamic mode returns a handle for runtime task management:
///
/// ```rust,no_run
/// # use taskvisor::prelude::*;
/// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
/// let handle = supervisor.serve();
///
/// // handle.add(...).await?;
/// // handle.cancel(id).await?;
/// // handle.shutdown().await?;
/// # Ok(()) }
/// ```
///
/// # Also
///
/// - [`SupervisorHandle`](crate::SupervisorHandle) - dynamic runtime management API
/// - [`SupervisorBuilder`](crate::SupervisorBuilder) - step-by-step construction
/// - [`SupervisorConfig`] - runtime defaults and limits
/// - [`TaskDefaults`](crate::TaskDefaults) - restart, backoff, timeout, and retry defaults
pub struct Supervisor {
    owner: Arc<RuntimeOwner>,

    #[cfg(feature = "controller")]
    controller: Option<Arc<crate::controller::Controller>>,
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor")
            .field("core", self.owner.core())
            .finish_non_exhaustive()
    }
}

impl Supervisor {
    /// Creates a supervisor from already-built runtime parts.
    pub(super) fn from_parts(
        core: Arc<SupervisorCore>,
        #[cfg(feature = "controller")] controller: Option<Arc<crate::controller::Controller>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            owner: RuntimeOwner::new(core),
            #[cfg(feature = "controller")]
            controller,
        })
    }

    /// Starts the controller loop once, if a controller is configured.
    #[cfg(feature = "controller")]
    fn start_controller(&self) {
        if let Some(controller) = &self.controller {
            controller.run();
        }
    }

    /// Creates a new supervisor with config and subscribers.
    ///
    /// Task specifications use [`TaskDefaults::default`](crate::TaskDefaults::default).
    /// Use [`builder`](Self::builder) with
    /// [`with_task_defaults`](crate::SupervisorBuilder::with_task_defaults) to
    /// replace those defaults.
    ///
    /// The returned supervisor is not started yet.
    /// Call [`run`](Self::run) for a fixed task set or [`serve`](Self::serve) for dynamic management.
    pub fn new(cfg: SupervisorConfig, subscribers: Vec<Arc<dyn Subscribe>>) -> Arc<Self> {
        Self::builder(cfg).with_subscribers(subscribers).build()
    }

    /// Creates a builder for constructing a supervisor.
    ///
    /// ## Example
    ///
    /// ```rust
    /// use taskvisor::{Supervisor, SupervisorConfig};
    ///
    /// let supervisor = Supervisor::builder(SupervisorConfig::default())
    ///     .with_subscribers(vec![])
    ///     .build();
    /// ```
    pub fn builder(cfg: SupervisorConfig) -> SupervisorBuilder {
        SupervisorBuilder::new(cfg)
    }

    /// Starts the runtime and returns a handle for dynamic task management.
    ///
    /// Safe to call more than once.
    /// Runtime listeners are started once, and each call returns a handle to the same runtime.
    ///
    /// # Panics
    ///
    /// Panics when this call must start the runtime but no active Tokio runtime
    /// exists. A failed first call does not mark the supervisor as started, so
    /// it may be retried inside Tokio. Once started, later calls only create a
    /// handle and do not require the caller to be on a Tokio worker.
    #[must_use = "use the returned runtime handle to manage or shut down the supervisor"]
    pub fn serve(&self) -> super::handle::SupervisorHandle {
        self.owner.core().start();
        #[cfg(feature = "controller")]
        self.start_controller();
        let handle = super::handle::SupervisorHandle::new(Arc::clone(&self.owner));
        #[cfg(feature = "controller")]
        let handle = handle.with_controller(self.controller.clone());
        handle
    }

    /// Runs a fixed task set until natural completion or OS shutdown signal.
    ///
    /// This is static mode.
    /// It validates and registers the provided tasks as one atomic batch, then waits until all tasks complete or a shutdown signal is received.
    /// If one task name conflicts, no task from the batch starts.
    ///
    /// A rejected batch does not stop tasks that were already registered through [`serve`](Self::serve).
    /// The runtime stays open for dynamic management, but `run` remains single-shot and cannot be called again.
    ///
    /// `run` is single-shot for one supervisor instance.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::GraceExceeded`] when some tasks did not stop within the grace period.
    /// - [`RuntimeError::TaskAlreadyExists`] when a task name is already in use or repeated in the batch.
    /// - [`RuntimeError::SignalSetupFailed`] when OS signal handlers cannot be installed.
    /// - [`RuntimeError::AlreadyRunning`] when `run` is called a second time.
    pub async fn run(&self, tasks: Vec<TaskSpec>) -> Result<(), RuntimeError> {
        #[cfg(feature = "controller")]
        self.start_controller();
        self.owner.core().run(tasks).await
    }

    /// Returns the immutable runtime configuration.
    #[must_use = "use the returned runtime configuration"]
    pub fn runtime_config(&self) -> &SupervisorConfig {
        self.owner.core().runtime_config()
    }

    /// Returns the immutable task defaults applied during registry admission.
    #[must_use = "use the returned task defaults"]
    pub fn task_defaults(&self) -> &crate::TaskDefaults {
        self.owner.core().task_defaults()
    }

    /// Returns the runtime core for controller tests.
    #[cfg(test)]
    pub(crate) fn core(&self) -> &Arc<SupervisorCore> {
        self.owner.core()
    }

    /// Returns the public-owner lease for controller unit tests.
    #[cfg(all(test, feature = "controller"))]
    pub(crate) fn owner(&self) -> &Arc<RuntimeOwner> {
        &self.owner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn last_public_owner_drop_releases_the_runtime_core() {
        let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
        let weak = Arc::downgrade(supervisor.core());
        let handle = supervisor.serve();

        drop(supervisor);
        assert!(weak.upgrade().is_some(), "the live handle owns the runtime");
        drop(handle);

        tokio::time::timeout(Duration::from_secs(2), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("last-owner Drop must not leave a core ownership cycle");
    }
}
