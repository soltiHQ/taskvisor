//! # Start and own the runtime
//!
//! [`Supervisor`] is the main entry point.
//! Build it once, then use `run` for a static batch or `serve` for dynamic management.
//! The modes can be combined by calling `serve` before the single `run` call.
//!
//! ## Modes
//!
//! ```text
//! static:   Supervisor::run(batch)  -> wait -> cleanup -> Result
//! dynamic:  Supervisor::serve()     -> SupervisorHandle
//!                                      | add / remove / cancel
//!                                      + shutdown -> cleanup -> Result
//! ```
//!
//! [`run`](Supervisor::run) is for a known initial batch.
//! [`serve`](Supervisor::serve) is for tasks managed while the service runs.
//!
//! After its batch is accepted, `run` waits for natural completion, explicit shutdown, or an OS shutdown signal.
//! `serve` does not install a signal wait; the application decides when to call [`SupervisorHandle::shutdown`](crate::SupervisorHandle::shutdown).
//!
//! ## Ownership and Drop
//!
//! `Supervisor` and all [`SupervisorHandle`](crate::SupervisorHandle) values share the runtime.
//! Dropping one owner does nothing while another owner is alive.
//! Dropping the last owner sends best-effort cancellation, but `Drop` cannot wait.
//!
//! Call [`SupervisorHandle::shutdown`](crate::SupervisorHandle::shutdown) to wait for cleanup and get its result.
//!
//! [`SupervisorCore`]: crate::core::SupervisorCore

use std::sync::Arc;

use crate::core::{RuntimeOwner, SupervisorConfig, SupervisorCore, builder::SupervisorBuilder};
use crate::{error::RuntimeError, subscribers::Subscribe, tasks::TaskSpec};

/// Owner and entry point for one taskvisor runtime.
///
/// Use [`new`](Self::new) for the standard defaults.
/// Use [`builder`](Self::builder) to set [`TaskDefaults`](crate::TaskDefaults), subscribers, or optional controller admission.
///
/// ## See Also
///
/// - [`SupervisorHandle`](crate::SupervisorHandle) - dynamic runtime management API
/// - [`SupervisorBuilder`] - step-by-step construction
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

    /// Creates a stopped supervisor with runtime config and subscribers.
    ///
    /// Task specs use [`TaskDefaults::default`](crate::TaskDefaults::default).
    /// Use [`builder`](Self::builder) and [`with_task_defaults`](crate::SupervisorBuilder::with_task_defaults) to replace those defaults.
    ///
    /// This method does not start Tokio tasks. Call [`run`](Self::run) or [`serve`](Self::serve) later.
    pub fn new(cfg: SupervisorConfig, subscribers: Vec<Arc<dyn Subscribe>>) -> Arc<Self> {
        Self::builder(cfg).with_subscribers(subscribers).build()
    }

    /// Creates a builder for custom supervisor settings.
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

    /// Starts dynamic mode and returns a management handle.
    ///
    /// This method may be called more than once.
    /// Runtime workers start once; every call returns another handle to the same runtime.
    ///
    /// # Panics
    ///
    /// Panics if the runtime must start and there is no active Tokio runtime.
    /// A failed first call may be retried inside Tokio.
    /// After startup, later calls only create a handle.
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

    /// Runs an initial task batch until natural completion or shared shutdown,
    /// started explicitly or by an OS signal.
    ///
    /// This is static mode.
    /// The registry accepts the full batch or rejects it.
    /// If a name is repeated or already registered, no task from the batch starts.
    ///
    /// `run` can be called only once for a supervisor.
    /// A rejected batch does not stop tasks that were added earlier through [`serve`](Self::serve), but the `run` call is still used and cannot be retried.
    ///
    /// # Panics
    ///
    /// Panics if the runtime must start and there is no active Tokio runtime.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::GraceExceeded`] when some tasks did not stop within the grace period.
    /// - [`RuntimeError::TaskAlreadyExists`] when a task name is already in use or repeated in the batch.
    /// - [`RuntimeError::SignalSetupFailed`] when OS signal handlers cannot be installed.
    /// - [`RuntimeError::AlreadyRunning`] when `run` is called a second time.
    /// - [`RuntimeError::ShuttingDown`] when shutdown has started or cleanup cannot finish normally.
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
