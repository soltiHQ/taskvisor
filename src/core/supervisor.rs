//! # Start and own the runtime
//!
//! [`Supervisor`] is the main entry point.
//! Build it once, then use `run` for a static batch or `serve` for dynamic management.
//! The modes can be combined by calling `serve` before the single `run` call.
//!
//! ## Modes
//!
//! ```text
//! static:   Supervisor::run(batch) ──► wait ──► cleanup ──► Result
//! dynamic:  Supervisor::serve() ─────► SupervisorHandle
//!                                        ├── add / remove / cancel
//!                                        └── shutdown ──► cleanup ──► Result
//! ```
//!
//! [`run`](Supervisor::run) is for a known initial batch.
//! [`serve`](Supervisor::serve) is for tasks managed while the service runs.
//!
//! After its batch is accepted, `run` waits for natural completion or shared shutdown.
//! Use [`run_until`](Supervisor::run_until) with an application-owned shutdown future.
//! Use [`run_with_os_signals`](Supervisor::run_with_os_signals) only when taskvisor should explicitly install process signal handlers.
//! `serve` does not install a signal wait; the application decides when to call [`SupervisorHandle::shutdown`](crate::SupervisorHandle::shutdown).
//!
//! ## Ownership and Drop
//!
//! `Supervisor` and all [`SupervisorHandle`](crate::SupervisorHandle) values share the runtime.
//! Dropping one owner does nothing while another owner is alive.
//! Dropping the last owner sends best-effort cancellation, but `Drop` cannot wait.
//!
//! Call [`SupervisorHandle::shutdown`](crate::SupervisorHandle::shutdown) to wait for the bounded shutdown workflow and get its result.
//! A force-reaped synchronous task, detached subscriber callback, or isolated user destructor may remain physically active afterward.
//!
//! [`SupervisorCore`]: crate::core::SupervisorCore

use std::{future::Future, sync::Arc};

use crate::core::{RuntimeOwner, SupervisorConfig, SupervisorCore, builder::SupervisorBuilder};
use crate::{error::RuntimeError, subscribers::Subscribe, tasks::TaskSpec};

/// Owner and entry point for one taskvisor runtime.
///
/// > Use [`new`](Self::new) for the standard defaults.
/// > Use [`builder`](Self::builder) to set [`TaskDefaults`](crate::TaskDefaults), subscribers, or optional controller admission.
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
    /// This method does not start Tokio tasks.
    /// Call [`run`](Self::run) or [`serve`](Self::serve) later.
    ///
    /// # Panics
    ///
    /// Panics when the process-wide library-owned user-lifetime budget cannot
    /// reserve one slot per subscriber, when a bounded async capacity is
    /// structurally too large, or when subscriber metadata panics.
    /// Use [`SupervisorBuilder::try_build`] for typed build errors.
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
    /// ## Example
    ///
    /// ```rust,no_run
    /// use taskvisor::prelude::*;
    ///
    /// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    /// let handle = supervisor.serve();
    ///
    /// let worker: TaskRef = TaskFn::arc(|ctx| async move {
    ///     ctx.cancelled().await;
    ///     Err(TaskError::Canceled)
    /// });
    ///
    /// let id = handle.add(TaskSpec::once("worker", worker)).await?;
    /// handle.cancel(id).await?;
    /// handle.shutdown().await?;
    /// # Ok(()) }
    /// ```
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

    /// Runs an initial task batch until natural completion or shared shutdown.
    ///
    /// This is static mode.
    /// The registry accepts the full batch or rejects it.
    /// If a name is repeated or already registered, no task from the batch starts.
    ///
    /// `run` can be called only once for a supervisor.
    /// A pre-start ownership-limit failure leaves the single-shot lifecycle unused, so a corrected batch may retry.
    /// Once the lifecycle reaches registry admission, a rejected batch does not stop tasks added earlier through [`serve`](Self::serve), but that `run` call is consumed and cannot be retried.
    ///
    /// `Ok(())` means the bounded supervisor lifecycle and cleanup workflow completed successfully.
    /// It does not prove physical exit of a force-reaped synchronous task, detached subscriber callback, or isolated user destructor.
    /// It does not mean that every managed task completed successfully.
    /// Task failures, fatal errors, and panics remain task-level outcomes; use [`SupervisorHandle::add_and_watch`](crate::SupervisorHandle::add_and_watch) when application logic needs a reliable outcome for one task.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use taskvisor::prelude::*;
    ///
    /// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    /// let task: TaskRef = TaskFn::arc(|_ctx| async move {
    ///     println!("one unit of work");
    ///     Ok(())
    /// });
    ///
    /// supervisor.run(vec![TaskSpec::once("worker", task)]).await?;
    /// # Ok(()) }
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if the runtime must start and there is no active Tokio runtime.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::GraceExceeded`] when some tasks did not stop within the grace period.
    /// - [`RuntimeError::TaskAlreadyExists`] when a task name is already in use or repeated in the batch.
    /// - [`RuntimeError::AlreadyRunning`] when `run` is called a second time.
    /// - [`RuntimeError::ShuttingDown`] when shutdown has started or cleanup cannot finish normally.
    pub async fn run(&self, tasks: Vec<TaskSpec>) -> Result<(), RuntimeError> {
        #[cfg(feature = "controller")]
        self.start_controller();
        self.owner.core().run(tasks).await
    }

    /// Runs an initial task batch until natural completion, shared shutdown, or an application-owned shutdown future completes.
    ///
    /// The shutdown future is polled while the initial batch waits for bounded
    /// ownership and registry admission. When it
    /// completes first, taskvisor starts the same graceful shutdown used by
    /// [`SupervisorHandle::shutdown`](crate::SupervisorHandle::shutdown).
    ///
    /// The future must resolve to `()`.
    /// Wrap a fallible source so the application decides how to handle its error before requesting shutdown.
    ///
    /// This method does not install process signal handlers.
    /// It is single-shot and shares the same one-call limit as [`run`](Self::run) and [`run_with_os_signals`](Self::run_with_os_signals).
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use taskvisor::prelude::*;
    ///
    /// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    /// let task: TaskRef = TaskFn::arc(|ctx| async move {
    ///     ctx.cancelled().await;
    ///     Err(TaskError::Canceled)
    /// });
    /// let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    ///
    /// tokio::spawn(async move {
    ///     let _ = stop.send(());
    /// });
    /// supervisor
    ///     .run_until(vec![TaskSpec::once("worker", task)], async move {
    ///         let _ = stopped.await;
    ///     })
    ///     .await?;
    /// # Ok(()) }
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if the runtime must start and there is no active Tokio runtime.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::GraceExceeded`] when some tasks did not stop within the grace period.
    /// - [`RuntimeError::TaskAlreadyExists`] when a task name is already in use or repeated in the batch.
    /// - [`RuntimeError::AlreadyRunning`] when any static run method is called a second time.
    /// - [`RuntimeError::ShuttingDown`] when shutdown has started or cleanup cannot finish normally.
    pub async fn run_until<F>(&self, tasks: Vec<TaskSpec>, shutdown: F) -> Result<(), RuntimeError>
    where
        F: Future<Output = ()>,
    {
        #[cfg(feature = "controller")]
        self.start_controller();
        self.owner.core().run_until(tasks, shutdown).await
    }

    /// Runs an initial task batch with explicit OS-signal shutdown handling.
    ///
    /// On Unix this waits for SIGINT, SIGTERM, or SIGQUIT.
    /// On other platforms it waits for Tokio's Ctrl-C signal.
    /// A received signal starts graceful shutdown.
    ///
    /// # Process-wide side effect
    ///
    /// Calling this method explicitly installs process-global Tokio signal handlers.
    /// On Unix, dropping the signal listeners does not restore the default signal disposition.
    /// The application remains responsible for signal handling after this method returns.
    ///
    /// Use [`run`](Self::run) or [`run_until`](Self::run_until) when the surrounding application owns process signals.
    ///
    /// This method is single-shot and shares the same one-call limit as the other static run methods.
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
    /// - [`RuntimeError::AlreadyRunning`] when any static run method is called a second time.
    /// - [`RuntimeError::ShuttingDown`] when shutdown has started or cleanup cannot finish normally.
    pub async fn run_with_os_signals(&self, tasks: Vec<TaskSpec>) -> Result<(), RuntimeError> {
        #[cfg(feature = "controller")]
        self.start_controller();
        self.owner.core().run_with_os_signals(tasks).await
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
