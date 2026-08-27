//! Sends management operations to one running supervisor.
//!
//! [`Supervisor::serve`](crate::Supervisor::serve) starts the runtime and returns a [`SupervisorHandle`].
//! State-changing methods create typed operations.
//! Building or configuring an operation has no effect.
//! Awaiting a default operation directly, or calling `execute` or `try_intake`, commits the
//! request.
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
/// Their modifiers select watched results, admission behavior, or a cancellation deadline without multiplying methods on this handle.
/// Await a default waiting, unwatched `add` or controller `submit` operation directly.
/// Call `execute().await` for configured operations and for `remove` or `cancel`; controller
/// submission also offers synchronous `try_intake()`.
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

    /// Creates a direct registry-registration operation.
    ///
    /// The default operation waits for cleanup ownership, registry command capacity, and the authoritative registration decision.
    /// Awaiting the default operation directly or successfully calling `execute` returns the registered
    /// [`TaskId`]. Use `watch()` to receive a [`TaskWaiter`](crate::TaskWaiter),
    /// `ownership_timeout(duration)` to bound only cleanup-ownership admission, or `fail_fast()`
    /// to require immediately available ownership and command capacity.
    ///
    /// Direct adds bypass controller slot admission.
    #[must_use = "await the default add or configure and execute the operation"]
    #[inline]
    pub fn add(&self, spec: TaskSpec) -> AddOperation<'_, Unwatched, Waiting> {
        AddOperation::new(self.core(), spec)
    }

    /// Creates a non-waiting removal operation for a task identity or registered name.
    ///
    /// `target` accepts [`TaskId`], `&str`, `String`, `Arc<str>`, and [`TaskTarget`]. Successful
    /// `execute` returns whether this call claimed removal. Registered task cleanup may continue
    /// after return; queued controller work is removed before return. Use `fail_fast()` to require
    /// immediately available management-queue capacity.
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

    /// Creates a terminal cancellation operation for a task identity or registered name.
    ///
    /// The default `execute` waits for management-queue capacity and logical terminal cleanup.
    /// Use `fail_fast()` to require immediate queue capacity and `termination_timeout(duration)`
    /// to limit only this caller's later cleanup wait. The two modifiers are independent and can
    /// be applied in either order. A termination timeout does not undo cancellation.
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

    /// Returns the authoritative registry view as `(id, name)` pairs.
    ///
    /// The result is sorted by [`TaskId`] and includes entries that are running, waiting for an
    /// attempt permit, in retry backoff, or completing cleanup. Concurrent changes can make the
    /// returned snapshot stale immediately.
    pub async fn list(&self) -> Vec<(TaskId, Arc<str>)> {
        self.core().list_tasks().await
    }

    /// Returns sorted task names whose physical attempt is still active.
    ///
    /// This includes force-aborted attempts that have left registry membership but have not
    /// physically exited. Concurrent changes can make the returned snapshot stale immediately.
    pub async fn alive_snapshot(&self) -> Vec<Arc<str>> {
        self.core().snapshot().await
    }

    /// Returns whether this registered name still has a physical attempt in progress.
    ///
    /// Waiting for a permit, retry backoff, or terminal cleanup is not active work. A
    /// force-aborted attempt can remain active after registry membership ends.
    pub async fn is_alive(&self, name: &str) -> bool {
        self.core().is_alive(name).await
    }

    /// Returns ownership-admission and deferred-cleanup state.
    ///
    /// Accepted task or subscriber values remain charged until their final isolated destruction
    /// finishes, including after registry membership and physical attempts end. The returned
    /// point-in-time view can become stale immediately.
    #[must_use = "inspect the returned ownership state"]
    pub fn ownership_snapshot(&self) -> OwnershipSnapshot {
        self.core().ownership_snapshot()
    }

    /// Returns the immutable runtime configuration.
    #[must_use = "inspect the returned runtime configuration"]
    pub fn runtime_config(&self) -> &crate::SupervisorConfig {
        self.core().runtime_config()
    }

    /// Returns the immutable task defaults applied during registry admission.
    #[must_use = "inspect the returned task defaults"]
    pub fn task_defaults(&self) -> &crate::TaskDefaults {
        self.core().task_defaults()
    }

    /// Closes admission and waits for the shared bounded cleanup workflow.
    ///
    /// Shutdown drains accepted controller work when configured, cancels registered tasks, waits
    /// through the grace window, joins runtime management workers, and drains subscriber queues up
    /// to their deadline. A force-aborted synchronous task, detached subscriber callback, or
    /// isolated user destructor may still be active after return.
    ///
    /// This consumes only the current handle value. Shutdown affects the shared runtime and every
    /// clone. Concurrent or later calls receive the same cached result.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::GraceExceeded`] when tasks miss the grace deadline and the other
    /// documented runtime shutdown errors when shared cleanup cannot finish normally.
    ///
    /// # Cancel safety
    ///
    /// On its first poll this method creates or joins a detached shared shutdown operation.
    /// Dropping this caller's future after that point does not stop cleanup.
    #[doc(alias = "graceful shutdown")]
    #[doc(alias = "graceful stop")]
    pub async fn shutdown(self) -> Result<(), RuntimeError> {
        self.core().shutdown().await
    }

    /// Allocates a controller submission identity before command intake or events.
    ///
    /// Preparation reserves no task name, slot, queue capacity, or runtime capacity. Record
    /// [`PreparedSubmission::id`](crate::PreparedSubmission::id), then call `submit()` on the
    /// prepared value to create the same typed submission operation as [`submit`](Self::submit).
    ///
    /// # Errors
    ///
    /// Returns [`ControllerError::NotConfigured`](crate::ControllerError::NotConfigured) when
    /// this supervisor was built without a controller.
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

    /// Creates a controller command-intake operation.
    ///
    /// Awaiting the default operation directly or calling `execute` waits for cleanup ownership
    /// and controller command capacity, then returns a reserved [`TaskId`]. It confirms command
    /// intake only; slot and registry admission happen later. Use `watch()` to return a
    /// final-outcome waiter, `ownership_timeout(duration)` to bound only cleanup-ownership
    /// admission, or synchronous `try_intake()` to require immediately available ownership and
    /// command capacity.
    ///
    /// A supervisor without a controller reports
    /// [`ControllerError::NotConfigured`](crate::ControllerError::NotConfigured) from the terminal
    /// method. Building or dropping the operation allocates no task identity and sends no command.
    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    #[must_use = "await the default submission or configure and execute the operation"]
    #[inline]
    pub fn submit(&self, spec: crate::controller::ControllerSpec) -> crate::controller::Submit<'_> {
        crate::controller::Submit::direct(self.controller.as_deref(), spec)
    }

    /// Returns a best-effort rolling snapshot of controller slots.
    ///
    /// Slots are copied one at a time, so concurrent changes can appear in only part of one
    /// snapshot. Returns `None` when this supervisor was built without a controller.
    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    pub async fn controller_snapshot(&self) -> Option<crate::controller::ControllerSnapshot> {
        match &self.controller {
            Some(controller) => Some(controller.snapshot().await),
            None => None,
        }
    }
}
