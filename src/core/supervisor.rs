//! Defines the public owner and lifecycle entry points for one Taskvisor runtime.
//!
//! [`SupervisorBuilder`] creates a stopped [`Supervisor`].
//! Static and dynamic lifecycles share the same runtime workers.
//!
//! ```text
//! stopped Supervisor
//!      ├── serve ──► SupervisorHandle ──► dynamic management
//!      └── run* ───► initial batch ─────► registry empty or stop trigger
//!                                             ▼
//!                                      shutdown cleanup
//! ```
//!
//! [`serve`](Supervisor::serve) can return multiple handles to the same running runtime.
//! The static `run*` lifecycle can be committed once and may be used after `serve`.
//! Natural shutdown starts when the entire registry becomes empty.
//! Tasks already registered through a handle participate in that boundary.
//!
//! [`Supervisor`] and all [`SupervisorHandle`](crate::SupervisorHandle) values share public ownership.
//! Dropping the last owner requests best-effort cancellation without waiting.
//! Use [`SupervisorHandle::shutdown`](crate::SupervisorHandle::shutdown) for the bounded shutdown result.

use std::{future::Future, sync::Arc};

use crate::core::{
    OwnershipSnapshot, RuntimeOwner, SupervisorConfig, SupervisorCore, builder::SupervisorBuilder,
};
use crate::{error::RuntimeError, subscribers::Subscribe, tasks::TaskSpec};

/// Public owner and startup entry point for one runtime.
///
/// Choose the lifecycle from how the application supplies work and requests shutdown:
///
/// - [`run_with_os_signals`](Self::run_with_os_signals) for Taskvisor-owned signal listeners;
/// - [`run_until`](Self::run_until) for an application-owned stop future;
/// - [`run`](Self::run) for an initial batch that ends naturally;
/// - [`serve`](Self::serve) for dynamic task management through a handle.
///
/// Use [`new`](Self::new) for default task settings.
/// Use [`builder`](Self::builder) to configure task defaults, subscribers, controller admission, or fallible construction.
/// Starting the runtime requires an active Tokio runtime.
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

    /// Stopped supervisor with runtime configuration and best-effort subscribers.
    ///
    /// Task specs use [`TaskDefaults::default`](crate::TaskDefaults::default).
    /// Use [`builder`](Self::builder) and [`with_task_defaults`](crate::SupervisorBuilder::with_task_defaults) to replace those defaults.
    ///
    /// This method does not start Tokio tasks. A non-empty subscriber list starts native cleanup workers during construction.
    /// Subscriber callback workers start only with the supervisor runtime. Call [`run`](Self::run) or [`serve`](Self::serve) later.
    /// Use [`SupervisorBuilder::try_build`] when construction failure must be handled instead of converted to a panic.
    ///
    /// # Panics
    ///
    /// With configured subscribers, panics when background cleanup workers cannot start or the configured ownership limit cannot admit every subscriber.
    /// It also panics when a channel or semaphore capacity is too large or subscriber metadata panics.
    /// Use [`SupervisorBuilder::try_build`] for typed build errors.
    pub fn new(cfg: SupervisorConfig, subscribers: Vec<Arc<dyn Subscribe>>) -> Arc<Self> {
        Self::builder(cfg).with_subscribers(subscribers).build()
    }

    /// Builder for custom supervisor settings.
    ///
    /// # Examples
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

    /// Dynamic runtime management through a [`SupervisorHandle`](crate::SupervisorHandle).
    ///
    /// Use this path when tasks are added, queried, or stopped while the application is running.
    /// It installs no process signal listeners.
    /// Call [`SupervisorHandle::shutdown`](crate::SupervisorHandle::shutdown) to close admission and wait for bounded cleanup.
    ///
    /// This method may be called more than once.
    /// Runtime workers start once.
    /// After successful startup, each call returns another handle to the same runtime.
    /// Shutdown is terminal.
    /// A later call does not restart workers or reopen admission.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use taskvisor::prelude::*;
    ///
    /// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
    /// let handle = supervisor.serve()?;
    ///
    /// let worker: TaskRef = TaskFn::arc(|ctx| async move {
    ///     ctx.cancelled().await;
    ///     Err(TaskError::Canceled)
    /// });
    ///
    /// let id = handle.add(TaskSpec::once("worker", worker)).await?;
    /// handle.cancel(id).execute().await?;
    /// handle.shutdown().await?;
    /// # Ok(()) }
    /// ```
    ///
    /// Failed startup may be retried.
    /// After startup, later calls only create another handle.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::TokioRuntimeUnavailable`] when first startup is requested outside a Tokio runtime;
    /// - [`RuntimeError::ThreadStartFailed`] when a required subscriber worker thread cannot be created.
    #[must_use = "use the returned runtime handle to manage or shut down the supervisor"]
    pub fn serve(&self) -> Result<super::handle::SupervisorHandle, RuntimeError> {
        self.owner.core().start()?;
        let handle = super::handle::SupervisorHandle::new(Arc::clone(&self.owner));
        #[cfg(feature = "controller")]
        let handle = handle.with_controller(self.controller.clone());
        Ok(handle)
    }

    /// Static lifecycle ending when registry membership becomes empty.
    ///
    /// Use this path when every managed task has a natural stopping condition.
    /// The registry accepts the full batch or rejects it.
    /// If a name is repeated or already registered, no task from the batch starts.
    /// Tasks already registered through [`serve`](Self::serve) also keep the registry non-empty and participate in this lifecycle.
    ///
    /// Static run methods share one committed lifecycle.
    /// An error before that commit leaves it available for another call.
    /// A rejected registry batch consumes the lifecycle, but does not stop tasks added earlier through [`serve`](Self::serve).
    ///
    /// `Ok(())` means the bounded supervisor lifecycle and cleanup workflow completed successfully.
    /// It does not prove physical exit of force-aborted task code, detached subscriber callbacks, or isolated user destructors.
    /// It also does not mean every task completed successfully.
    /// Add `watch()` to [`SupervisorHandle::add`](crate::SupervisorHandle::add) when application logic needs one task's final outcome.
    ///
    /// # Examples
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
    /// # Errors
    ///
    /// - [`RuntimeError::ResourceLimitReached`] when existing registrations leave insufficient registry capacity for the complete initial batch;
    /// - [`RuntimeError::ResourceLimitReached`] when the complete initial batch exceeds the effective ownership capacity available after existing charges and permanent retirement;
    /// - [`RuntimeError::ThreadStartFailed`] when a required subscriber worker thread cannot be created;
    /// - [`RuntimeError::ThreadStartFailed`] when a required cleanup worker thread cannot be created;
    /// - [`RuntimeError::ThreadStartFailed`] when a required cleanup worker exits before completing its startup handshake;
    /// - [`RuntimeError::AlreadyRunning`] when another static run currently owns the lifecycle;
    /// - [`RuntimeError::AlreadyRunning`] when an earlier static run committed the lifecycle;
    /// - [`RuntimeError::TaskAlreadyExists`] when an initial task name is already reserved;
    /// - [`RuntimeError::TaskAlreadyExists`] when the initial batch repeats a task name;
    /// - [`RuntimeError::TokioRuntimeUnavailable`] when startup is requested outside a Tokio runtime;
    /// - [`RuntimeError::ShuttingDown`] when closed runtime intake prevents the initial batch from committing;
    /// - [`RuntimeError::ShuttingDown`] when the shared shutdown workflow cannot publish a clean terminal outcome;
    /// - [`RuntimeError::GraceExceeded`] when some tasks did not stop within the grace period.
    ///
    /// # Cancel safety
    ///
    /// Keep this future alive until it returns.
    /// Dropping it after the static lifecycle commits does not stop admitted tasks or start shutdown.
    /// A handle from [`serve`](Self::serve) can still request shutdown.
    pub async fn run(&self, tasks: Vec<TaskSpec>) -> Result<(), RuntimeError> {
        self.owner.core().run(tasks).await
    }

    /// Static lifecycle with an application-owned shutdown future.
    ///
    /// The shutdown future can win before registry command commit or after successful batch admission.
    /// A win at either point uses the graceful shutdown of [`SupervisorHandle::shutdown`](crate::SupervisorHandle::shutdown).
    /// The initial batch may not start when the future wins before registry admission commits.
    ///
    /// The future must resolve to `()`.
    /// Wrap a fallible source and handle its error before requesting shutdown.
    ///
    /// This method does not install process signal handlers.
    /// It shares the commit and cancel-safety behavior described by [`run`](Self::run).
    ///
    /// # Examples
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
    /// # Errors
    ///
    /// - [`RuntimeError::ResourceLimitReached`] when existing registrations leave insufficient registry capacity for the complete initial batch;
    /// - [`RuntimeError::ResourceLimitReached`] when the complete initial batch exceeds the effective ownership capacity available after existing charges and permanent retirement;
    /// - [`RuntimeError::ThreadStartFailed`] when a required subscriber worker thread cannot be created;
    /// - [`RuntimeError::ThreadStartFailed`] when a required cleanup worker thread cannot be created;
    /// - [`RuntimeError::ThreadStartFailed`] when a required cleanup worker exits before completing its startup handshake;
    /// - [`RuntimeError::AlreadyRunning`] when another static run currently owns the lifecycle;
    /// - [`RuntimeError::AlreadyRunning`] when an earlier static run committed the lifecycle;
    /// - [`RuntimeError::TaskAlreadyExists`] when an initial task name is already reserved;
    /// - [`RuntimeError::TaskAlreadyExists`] when the initial batch repeats a task name;
    /// - [`RuntimeError::TokioRuntimeUnavailable`] when startup is requested outside a Tokio runtime;
    /// - [`RuntimeError::ShuttingDown`] when closed runtime intake prevents the initial batch from committing;
    /// - [`RuntimeError::ShuttingDown`] when the shared shutdown workflow cannot publish a clean terminal outcome;
    /// - [`RuntimeError::GraceExceeded`] when some tasks did not stop within the grace period.
    pub async fn run_until<F>(&self, tasks: Vec<TaskSpec>, shutdown: F) -> Result<(), RuntimeError>
    where
        F: Future<Output = ()>,
    {
        self.owner.core().run_until(tasks, shutdown).await
    }

    /// Static lifecycle with Taskvisor-owned operating-system signal handling.
    ///
    /// On Unix this waits for SIGINT, SIGTERM, or SIGQUIT.
    /// Other platforms use Tokio's Ctrl-C signal.
    /// A received signal starts graceful shutdown.
    /// Failure to install a listener closes admission and runs the common
    /// cleanup tail without the normal task grace drain.
    ///
    /// # Process-wide side effect
    ///
    /// Calling this method explicitly installs process-global Tokio signal handlers.
    /// On Unix, dropping the signal listeners does not restore the default signal disposition.
    /// The application remains responsible for signal handling after this method returns.
    ///
    /// Use [`run`](Self::run) or [`run_until`](Self::run_until) when the surrounding application owns process signals.
    ///
    /// This method shares the commit and cancel-safety behavior described by [`run`](Self::run).
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::ResourceLimitReached`] when existing registrations leave insufficient registry capacity for the complete initial batch;
    /// - [`RuntimeError::ResourceLimitReached`] when the complete initial batch exceeds the effective ownership capacity available after existing charges and permanent retirement;
    /// - [`RuntimeError::ThreadStartFailed`] when a required subscriber worker thread cannot be created;
    /// - [`RuntimeError::ThreadStartFailed`] when a required cleanup worker thread cannot be created;
    /// - [`RuntimeError::ThreadStartFailed`] when a required cleanup worker exits before completing its startup handshake;
    /// - [`RuntimeError::AlreadyRunning`] when another static run currently owns the lifecycle;
    /// - [`RuntimeError::AlreadyRunning`] when an earlier static run committed the lifecycle;
    /// - [`RuntimeError::TaskAlreadyExists`] when an initial task name is already reserved;
    /// - [`RuntimeError::TaskAlreadyExists`] when the initial batch repeats a task name;
    /// - [`RuntimeError::TokioRuntimeUnavailable`] when startup is requested outside a Tokio runtime;
    /// - [`RuntimeError::ShuttingDown`] when closed runtime intake prevents the initial batch from committing;
    /// - [`RuntimeError::ShuttingDown`] when the shared shutdown workflow cannot publish a clean terminal outcome;
    /// - [`RuntimeError::GraceExceeded`] when some tasks did not stop within the grace period;
    /// - [`RuntimeError::SignalSetupFailed`] when the signal handlers cannot be installed.
    pub async fn run_with_os_signals(&self, tasks: Vec<TaskSpec>) -> Result<(), RuntimeError> {
        self.owner.core().run_with_os_signals(tasks).await
    }

    /// Immutable runtime configuration.
    #[must_use = "use the returned runtime configuration"]
    pub fn runtime_config(&self) -> &SupervisorConfig {
        self.owner.core().runtime_config()
    }

    /// Point-in-time ownership-admission and deferred-cleanup state.
    ///
    /// Calling this on a stopped supervisor does not start destructor workers.
    /// Configured subscribers are already reflected because their ownership is reserved during construction.
    /// The returned point-in-time view can become stale immediately.
    #[must_use = "inspect the returned ownership state"]
    pub fn ownership_snapshot(&self) -> OwnershipSnapshot {
        self.owner.core().ownership_snapshot()
    }

    /// Immutable task defaults applied during registry admission.
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
    use std::{
        pin::pin,
        task::{Context, Poll, Waker},
        time::Duration,
    };

    fn poll_ready_without_runtime<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("startup failure must resolve without an active runtime"),
        }
    }

    #[test]
    fn serve_without_tokio_returns_typed_error_and_can_retry() {
        let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
        assert!(!supervisor.core().drop_domain().is_started());
        assert!(matches!(
            supervisor.serve(),
            Err(RuntimeError::TokioRuntimeUnavailable)
        ));
        assert!(!supervisor.core().drop_domain().is_started());

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            let handle = supervisor.serve().expect("retry inside Tokio must start");
            assert!(!supervisor.core().drop_domain().is_started());
            handle.shutdown().await.expect("runtime must shut down");
            assert!(!supervisor.core().drop_domain().is_started());
        });
    }

    #[test]
    fn failed_static_start_does_not_consume_single_shot_lifecycle() {
        let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
        assert!(!supervisor.core().drop_domain().is_started());
        for _ in 0..2 {
            assert!(matches!(
                poll_ready_without_runtime(supervisor.run(vec![])),
                Err(RuntimeError::TokioRuntimeUnavailable)
            ));
            assert!(!supervisor.core().drop_domain().is_started());
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            supervisor
                .run(vec![])
                .await
                .expect("retry inside Tokio must retain the run lifecycle");
            assert!(!supervisor.core().drop_domain().is_started());
        });
    }

    #[test]
    fn nonempty_static_without_tokio_keeps_destructor_isolation_dormant() {
        let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
        let task = crate::TaskFn::arc(|_ctx| async { Ok(()) });
        assert!(matches!(
            poll_ready_without_runtime(supervisor.run(vec![TaskSpec::once("outside-tokio", task)])),
            Err(RuntimeError::TokioRuntimeUnavailable)
        ));
        assert!(!supervisor.core().drop_domain().is_started());
    }

    #[tokio::test]
    async fn last_public_owner_drop_releases_the_runtime_core() {
        let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
        let weak = Arc::downgrade(supervisor.core());
        let handle = supervisor.serve().expect("runtime startup");

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
