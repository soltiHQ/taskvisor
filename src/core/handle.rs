//! Defines management operations for one running supervisor.
//!
//! [`Supervisor::serve`](crate::Supervisor::serve) starts the runtime and returns a [`SupervisorHandle`].
//! Typed operations carry configuration without changing runtime state.
//! Direct await, `execute`, and `try_intake` are terminal operations.
//!
//! ```text
//! application ──► SupervisorHandle
//!                       ├── add/remove/cancel builder ──► registry management
//!                       ├── submit builder ─────────────► controller admission
//!                       └── shutdown ───────────────────► shared shutdown workflow
//! ```

use std::sync::Arc;

use crate::core::{
    AddOperation, CancelOperation, OwnershipSnapshot, RemoveOperation, RuntimeOwner,
    SupervisorCore, TaskTarget, TerminationUnbounded, Unwatched, Waiting,
};
use crate::error::RuntimeError;
use crate::identity::TaskId;
use crate::tasks::TaskSpec;

/// Cloneable API for managing one running supervisor.
///
/// `add`, `remove`, `cancel`, and controller `submit` return single-use operation builders.
/// Their modifiers select watched results, admission behavior, or a cancellation deadline.
/// Await a default waiting, unwatched `add` or controller `submit` operation directly.
/// Call `execute().await` for configured operations and for `remove` or `cancel`.
/// Controller submission also offers synchronous `try_intake()`.
///
/// Once a terminal method commits its queue command, the runtime owns that command even if the caller drops the returned future.
///
/// Every clone keeps the runtime publicly owned.
/// Dropping the last public owner requests best-effort cancellation but cannot wait.
/// Call [`shutdown`](Self::shutdown) when the final shutdown result is required.
/// Starting shutdown closes admission for every clone.
///
/// # Examples
///
/// ```rust,no_run
/// use std::time::Duration;
/// use taskvisor::{Supervisor, SupervisorConfig, TaskFn, TaskSpec};
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
///     let handle = supervisor.serve()?;
///
///     let task = TaskFn::arc(|ctx| async move {
///         loop {
///             ctx.run_until_cancelled(tokio::time::sleep(Duration::from_secs(1)))
///                 .await?;
///         }
///     });
///
///     let id = handle.add(TaskSpec::restartable("worker", task)).await?;
///     let _claimed = handle.cancel(id).execute().await?;
///     handle.shutdown().await?;
///     Ok(())
/// }
/// ```
#[derive(Clone)]
pub struct SupervisorHandle {
    owner: Arc<RuntimeOwner>,

    #[cfg(feature = "controller")]
    controller: Option<Arc<crate::controller::Controller>>,
}

impl std::fmt::Debug for SupervisorHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupervisorHandle")
            .field("core", self.owner.core())
            .finish_non_exhaustive()
    }
}

impl SupervisorHandle {
    /// Creates a new handle over an already-started runtime core.
    pub(crate) fn new(owner: Arc<RuntimeOwner>) -> Self {
        Self {
            owner,
            #[cfg(feature = "controller")]
            controller: None,
        }
    }

    pub(super) fn core(&self) -> &Arc<SupervisorCore> {
        self.owner.core()
    }

    /// Returns the configured controller without cloning its shared handle state.
    #[cfg(feature = "controller")]
    pub(super) fn controller(&self) -> Option<&crate::controller::Controller> {
        self.controller.as_deref()
    }

    /// Attaches the optional controller to this handle.
    #[cfg(feature = "controller")]
    pub(crate) fn with_controller(
        mut self,
        controller: Option<Arc<crate::controller::Controller>>,
    ) -> Self {
        self.controller = controller;
        self
    }

    /// Direct registry admission for one task specification.
    ///
    /// The default operation waits for cleanup ownership, registry command capacity, and the authoritative registration decision.
    /// Direct await and `execute` return the registered [`TaskId`].
    /// Use `watch()` for a [`TaskWaiter`](crate::TaskWaiter).
    /// Use `ownership_timeout(duration)` to bound only cleanup-ownership admission.
    /// Use `fail_fast()` when ownership and command capacity must be available immediately.
    ///
    /// Direct adds bypass controller slot admission.
    #[must_use = "await the default add or configure and execute the operation"]
    #[inline]
    pub fn add(&self, spec: TaskSpec) -> AddOperation<'_, Unwatched, Waiting> {
        AddOperation::new(self.core(), spec)
    }

    /// Non-waiting removal by task identity or registered name.
    ///
    /// `target` accepts [`TaskId`], `&str`, `String`, `Arc<str>`, and [`TaskTarget`].
    /// Successful `execute` reports whether this call claimed removal.
    /// Registered task cleanup may continue after return.
    /// Queued controller work is removed before return.
    /// Use `fail_fast()` when management-queue capacity must be available immediately.
    ///
    /// Identity targets pass through a configured controller and can reach queued submissions.
    /// Name targets resolve only registry membership.
    #[must_use = "configure and execute the remove operation"]
    #[inline]
    pub fn remove<Target>(&self, target: Target) -> RemoveOperation<'_, Waiting, Target>
    where
        Target: Into<TaskTarget>,
    {
        RemoveOperation::new(self, target)
    }

    /// Cancellation by task identity or registered name with logical cleanup confirmation.
    ///
    /// The default `execute` waits for management-queue capacity and logical terminal cleanup.
    /// Use `fail_fast()` when queue capacity must be available immediately.
    /// Use `termination_timeout(duration)` to limit only this caller's later cleanup wait.
    /// The two modifiers are independent and can be applied in either order.
    /// A termination timeout does not undo cancellation.
    ///
    /// Identity targets pass through a configured controller and can reach queued submissions.
    /// Name targets resolve only registry membership.
    #[must_use = "configure and execute the cancel operation"]
    #[inline]
    pub fn cancel<Target>(
        &self,
        target: Target,
    ) -> CancelOperation<'_, Waiting, TerminationUnbounded, Target>
    where
        Target: Into<TaskTarget>,
    {
        CancelOperation::new(self, target)
    }

    /// Authoritative point-in-time registry view as `(id, name)` pairs.
    ///
    /// The result is sorted by [`TaskId`] and includes entries that are running, waiting for an attempt permit, in retry backoff, or completing cleanup.
    /// Concurrent changes can make the returned snapshot stale immediately.
    pub async fn list(&self) -> Vec<(TaskId, Arc<str>)> {
        self.core().list_tasks().await
    }

    /// Sorted task names with a physically active attempt.
    ///
    /// This includes force-aborted attempts that have left registry membership but have not physically exited.
    /// Concurrent changes can make the returned snapshot stale immediately.
    pub async fn alive_snapshot(&self) -> Vec<Arc<str>> {
        self.core().snapshot().await
    }

    /// Whether a registered name has a physical attempt in progress.
    ///
    /// Waiting for a permit, retry backoff, or terminal cleanup is not active work.
    /// A force-aborted attempt can remain active after registry membership ends.
    pub async fn is_alive(&self, name: &str) -> bool {
        self.core().is_alive(name).await
    }

    /// Point-in-time ownership-admission and deferred-cleanup state.
    ///
    /// Accepted task or subscriber values remain charged until their final isolated destruction finishes.
    /// This can outlive registry membership and physical attempts.
    /// The returned point-in-time view can become stale immediately.
    #[must_use = "inspect the returned ownership state"]
    pub fn ownership_snapshot(&self) -> OwnershipSnapshot {
        self.core().ownership_snapshot()
    }

    /// Immutable runtime configuration.
    #[must_use = "inspect the returned runtime configuration"]
    pub fn runtime_config(&self) -> &crate::SupervisorConfig {
        self.core().runtime_config()
    }

    /// Immutable task defaults applied during registry admission.
    #[must_use = "inspect the returned task defaults"]
    pub fn task_defaults(&self) -> &crate::TaskDefaults {
        self.core().task_defaults()
    }

    /// Shared bounded shutdown for every handle clone.
    ///
    /// The configured grace window bounds registered task cleanup.
    /// The subscriber drain deadline bounds queued subscriber events.
    /// A force-aborted synchronous task, detached subscriber callback, or isolated user destructor may still be active after return.
    ///
    /// This consumes only the current handle value.
    /// Shutdown affects the shared runtime and every clone.
    /// Concurrent or later calls receive the same cached result.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::GraceExceeded`] when tasks miss the grace deadline;
    /// - [`RuntimeError::SignalSetupFailed`] when this call joins a shared shutdown whose operating-system signal setup failed;
    /// - [`RuntimeError::ShuttingDown`] when shared cleanup cannot finish normally.
    ///
    /// # Cancel safety
    ///
    /// On its first poll this method creates or joins a detached shared shutdown operation.
    /// Taskvisor schedules a new operation on the Tokio runtime that successfully started the supervisor.
    /// Polling from another runtime does not transfer cleanup ownership to that runtime.
    /// The startup runtime must remain alive and driven until the shared operation completes.
    /// Dropping this caller's future after that point does not stop cleanup.
    #[doc(alias = "graceful shutdown")]
    #[doc(alias = "graceful stop")]
    pub async fn shutdown(self) -> Result<(), RuntimeError> {
        self.core().shutdown().await
    }

    /// Controller submission identity available before command intake or events.
    ///
    /// Preparation reserves no task name, slot, queue capacity, or runtime capacity.
    /// Record [`PreparedSubmission::id`](crate::PreparedSubmission::id), then call `submit()` on the prepared value.
    /// The resulting operation has the same contract as [`submit`](Self::submit).
    ///
    /// # Errors
    ///
    /// - [`ControllerError::NotConfigured`](crate::ControllerError::NotConfigured) when this supervisor was built without a controller.
    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    pub fn prepare_submission(
        &self,
        spec: crate::controller::ControllerSpec,
    ) -> Result<crate::controller::PreparedSubmission, crate::controller::ControllerError> {
        match &self.controller {
            Some(controller) => Ok(crate::controller::PreparedSubmission::new(
                controller.handle(),
                spec,
            )),
            None => Err(crate::controller::ControllerError::NotConfigured),
        }
    }

    /// Controller command intake with optional final-outcome delivery.
    ///
    /// Direct await and `execute` wait for cleanup ownership and controller command capacity.
    /// Their [`TaskId`] confirms command intake only.
    /// Slot and registry admission happen later.
    /// Use `watch()` for a final-outcome waiter.
    /// Use `ownership_timeout(duration)` to bound only cleanup-ownership admission.
    /// Use synchronous `try_intake()` when ownership and command capacity must be available immediately.
    ///
    /// A supervisor without a controller reports [`ControllerError::NotConfigured`](crate::ControllerError::NotConfigured) from the terminal method.
    /// Building or dropping the operation allocates no task identity and sends no command.
    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    #[must_use = "await the default submission or configure and execute the operation"]
    #[inline]
    pub fn submit(&self, spec: crate::controller::ControllerSpec) -> crate::controller::Submit<'_> {
        crate::controller::Submit::direct(self.controller.as_deref(), spec)
    }

    /// Best-effort rolling snapshot of controller slots.
    ///
    /// Because slots are copied one at a time, concurrent changes can appear in only part of one snapshot.
    /// `None` means this supervisor was built without a controller.
    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    pub async fn controller_snapshot(&self) -> Option<crate::controller::ControllerSnapshot> {
        match &self.controller {
            Some(controller) => Some(controller.snapshot().await),
            None => None,
        }
    }
}
