//! Sends management operations to one running supervisor.
//!
//! [`Supervisor::serve`](crate::Supervisor::serve) starts the runtime and returns a [`SupervisorHandle`].
//! Direct task operations enter the registry management queue. With controller support,
//! submissions enter the controller queue first.
//!
//! ```text
//! application ──► SupervisorHandle
//!                       ├── task management ──► registry queue ──► Registry
//!                       ├── submit ──► controller queue ──► slot admission
//!                       │                                      ▼
//!                       │                                  Registry
//!                       └── shutdown ──► shared shutdown workflow
//! ```
//!
//! Regular state-changing methods wait for bounded queue capacity. Their `try_*` forms fail immediately
//! when capacity is unavailable. Direct replies carry registry and controller decisions outside the event path.
//!
//! Identity-based remove and cancel operations pass through the controller when configured. This orders
//! them after earlier submissions and lets them find work that has not reached the registry yet.
//! Methods ending in `_by_name` use the task name from [`TaskSpec`].

use std::{sync::Arc, time::Duration};

use crate::core::{OwnershipSnapshot, RuntimeOwner, SupervisorCore};
use crate::error::RuntimeError;
use crate::identity::TaskId;
use crate::tasks::TaskSpec;

use super::outcome::TaskWaiter;

/// Cloneable API for managing one running supervisor.
///
/// Choose an operation from the result the application needs:
///
/// - [`add`](Self::add) confirms registration; [`add_and_watch`](Self::add_and_watch) also returns the final outcome;
/// - [`remove`](Self::remove) starts a stop without waiting; [`cancel`](Self::cancel) waits for logical cleanup;
/// - [`list`](Self::list) reports membership; [`alive_snapshot`](Self::alive_snapshot) reports active attempts;
/// - controller `submit*` methods apply slot policy; direct `add*` methods do not.
///
/// Once a state-changing method commits its queue command, the runtime owns that command even if the caller drops its future.
///
/// Every clone keeps the runtime publicly owned. Dropping the last public owner requests best-effort cancellation but cannot wait.
/// Call [`shutdown`](Self::shutdown) to wait for the bounded shutdown workflow.
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
///             // Do one unit of work.
///         }
///     });
///
///     let id = handle.add(TaskSpec::restartable("worker", task)).await?;
///     let _claimed = handle.cancel(id).await?;
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

    fn core(&self) -> &Arc<SupervisorCore> {
        self.owner.core()
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

    /// Registers a task and waits for the registry's decision.
    ///
    /// `Ok(id)` confirms registration.
    /// It does not mean that the first attempt has started. This confirmation is direct and does not use the event bus.
    /// The task name must not already identify registry membership or a force-aborted task whose physical actor has not exited.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::ThreadStartFailed`] when background cleanup workers cannot start for the first ownership admission.
    /// - [`RuntimeError::ResourceLimitReached`] when the task exceeds a configured ownership or registered-task limit.
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    /// - [`RuntimeError::TaskAlreadyExists`] when the task name is already in use.
    pub async fn add(&self, spec: TaskSpec) -> Result<TaskId, RuntimeError> {
        self.core().add_task(spec).await
    }

    /// Registers a task after a bounded wait for ownership admission.
    ///
    /// `wait_for` covers only the cleanup-ownership permit. Once Taskvisor acquires that
    /// permit, command-queue admission and the registry decision follow [`add`](Self::add)
    /// without this deadline. An immediately available permit can succeed when `wait_for`
    /// is [`Duration::ZERO`]. The timer cannot interrupt synchronous lazy cleanup-worker startup.
    ///
    /// A timeout happens before command commit. It starts no task and publishes no lifecycle
    /// event for this request.
    ///
    /// # Errors
    ///
    /// Returns the errors from [`add`](Self::add).
    /// It also returns [`RuntimeError::OwnershipAdmissionTimeout`] when ownership remains unavailable for `wait_for`.
    pub async fn add_with_ownership_timeout(
        &self,
        spec: TaskSpec,
        wait_for: Duration,
    ) -> Result<TaskId, RuntimeError> {
        self.core()
            .add_task_with_ownership_timeout(spec, wait_for)
            .await
    }

    /// Registers a task without waiting for ownership admission.
    ///
    /// This is useful when the caller must apply its own overload policy instead of waiting for a configured ownership limit.
    /// After ownership admission, it still waits for the registry decision.
    /// `Ok(id)` has the same meaning as [`add`](Self::add).
    ///
    /// # Errors
    ///
    /// Returns the errors from [`add`](Self::add).
    /// It also returns [`RuntimeError::CommandQueueFull`] when the registry queue has no capacity.
    pub async fn try_add(&self, spec: TaskSpec) -> Result<TaskId, RuntimeError> {
        self.core().try_add_task(spec).await
    }

    /// Registers a task and returns a waiter for its final outcome.
    ///
    /// The return confirms the same registry admission as [`add`](Self::add).
    /// [`TaskWaiter`] uses a direct completion channel outside lifecycle events.
    /// Use this method when application behavior depends on how the task ends.
    ///
    /// # Errors
    ///
    /// Returns the same admission errors as [`add`](Self::add).
    pub async fn add_and_watch(
        &self,
        spec: TaskSpec,
    ) -> Result<(TaskId, TaskWaiter), RuntimeError> {
        let (id, done_rx) = self.core().add_task_watched(spec).await?;
        Ok((id, TaskWaiter::new(id, done_rx)))
    }

    /// Registers watched work after a bounded wait for ownership admission.
    ///
    /// The ownership-only deadline and post-timeout behavior match [`add_with_ownership_timeout`](Self::add_with_ownership_timeout).
    /// After ownership admission, registry confirmation and final-outcome behavior match [`add_and_watch`](Self::add_and_watch).
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`add_with_ownership_timeout`](Self::add_with_ownership_timeout).
    pub async fn add_and_watch_with_ownership_timeout(
        &self,
        spec: TaskSpec,
        wait_for: Duration,
    ) -> Result<(TaskId, TaskWaiter), RuntimeError> {
        let (id, done_rx) = self
            .core()
            .add_task_watched_with_ownership_timeout(spec, wait_for)
            .await?;
        Ok((id, TaskWaiter::new(id, done_rx)))
    }

    /// Registers watched work without waiting for ownership admission.
    ///
    /// After queue admission, registration and outcome behavior match [`add_and_watch`](Self::add_and_watch).
    ///
    /// # Errors
    ///
    /// Returns the errors from [`add_and_watch`](Self::add_and_watch).
    /// It also returns [`RuntimeError::CommandQueueFull`] when the registry queue has no capacity.
    pub async fn try_add_and_watch(
        &self,
        spec: TaskSpec,
    ) -> Result<(TaskId, TaskWaiter), RuntimeError> {
        let (id, done_rx) = self.core().try_add_task_watched(spec).await?;
        Ok((id, TaskWaiter::new(id, done_rx)))
    }

    /// Requests removal by task identity without waiting for termination.
    ///
    /// `Ok(true)` means this call claimed the task and sent cancellation, or removed it from the controller queue.
    /// `Ok(false)` means the identity was unknown, already finished, or already claimed by another stop request.
    ///
    /// For a registered task, the method returns before final cleanup.
    /// Removing queued controller work is complete when this method returns.
    /// Use [`cancel`](Self::cancel) when the caller needs terminal confirmation.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::ResourceLimitReached`] when a configured controller's identity-operation budget is full.
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn remove(&self, id: TaskId) -> Result<bool, RuntimeError> {
        #[cfg(feature = "controller")]
        if let Some(controller) = &self.controller {
            return controller.handle().remove(id).await;
        }
        self.core().remove(id).await
    }

    /// Requests removal only if the management queue has capacity now.
    ///
    /// After queue admission, it waits for the same decision and returns the same boolean as [`remove`](Self::remove).
    ///
    /// # Errors
    ///
    /// Returns the errors from [`remove`](Self::remove). It also returns [`RuntimeError::CommandQueueFull`]
    /// when a required management queue has no capacity.
    pub async fn try_remove(&self, id: TaskId) -> Result<bool, RuntimeError> {
        #[cfg(feature = "controller")]
        if let Some(controller) = &self.controller {
            return controller.handle().try_remove(id).await;
        }
        self.core().try_remove(id).await
    }

    /// Requests removal of the registered task with `name`.
    ///
    /// Name lookup and the removal claim are one registry operation.
    /// The boolean has the same meaning as [`remove`](Self::remove).
    /// This method also returns before final cleanup.
    ///
    /// Controller submissions that are still queued do not own a registered name.
    /// Remove them with the [`TaskId`] returned by `submit`.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn remove_by_name(&self, name: &str) -> Result<bool, RuntimeError> {
        self.core().remove_by_name(Arc::from(name)).await
    }

    /// Requests removal by name only if the registry queue has capacity now.
    ///
    /// After queue admission, behavior is the same as [`remove_by_name`](Self::remove_by_name).
    ///
    /// # Errors
    ///
    /// Returns the errors from [`remove_by_name`](Self::remove_by_name). It also returns
    /// [`RuntimeError::CommandQueueFull`] when the registry queue has no capacity.
    pub async fn try_remove_by_name(&self, name: &str) -> Result<bool, RuntimeError> {
        self.core().try_remove_by_name(Arc::from(name)).await
    }

    /// Returns the authoritative registry view as `(id, name)` pairs.
    ///
    /// The list comes from the registry and is sorted by [`TaskId`]. It includes every registry
    /// entry: running, waiting for a permit, between attempts, awaiting cleanup, or being removed.
    ///
    /// See [`alive_snapshot`](Self::alive_snapshot) for tasks currently executing an attempt.
    /// Concurrent lifecycle changes can make the returned snapshot stale immediately.
    pub async fn list(&self) -> Vec<(TaskId, Arc<str>)> {
        self.core().list_tasks().await
    }

    /// Returns task names that still have a physical attempt in progress.
    ///
    /// This combines activity from registry entries and force-aborted attempts that have not physically exited.
    /// A name remains in the result until its physical attempt exits. Event loss does not affect this result.
    /// Results are sorted by name.
    ///
    /// See [`list`](Self::list) for registry membership.
    /// Concurrent attempt changes can make the returned snapshot stale immediately.
    pub async fn alive_snapshot(&self) -> Vec<Arc<str>> {
        self.core().snapshot().await
    }

    /// Returns whether this name still has a physical attempt in progress.
    ///
    /// Waiting for a permit, retry backoff, or terminal cleanup is not active.
    /// A force-aborted attempt can remain active after registry membership ends.
    /// Use [`list`](Self::list) when registry membership is the desired state.
    pub async fn is_alive(&self, name: &str) -> bool {
        self.core().is_alive(name).await
    }

    /// Returns ownership-admission and deferred-cleanup state.
    ///
    /// This view is separate from [`list`](Self::list) and [`alive_snapshot`](Self::alive_snapshot).
    /// Accepted task or subscriber values remain charged until their final isolated destruction finishes,
    /// including after registry membership and physical attempts have ended.
    ///
    /// The returned point-in-time view can become stale immediately.
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

    /// Cancels work by identity and waits for bounded logical terminal cleanup.
    ///
    /// For registered work, this returns after registry membership is removed and the final outcome is committed.
    /// Except for [`TaskOutcome::ForceAborted`](crate::TaskOutcome::ForceAborted), the actor is physically joined first.
    /// A force-aborted actor can remain physically active until it exits.
    ///
    /// `Ok(true)` means this call created the stop claim. A call that joins an existing removal waits for
    /// the same cleanup and returns `Ok(false)`.
    /// Unknown or already-cleaned work also returns `Ok(false)`.
    /// Queued controller work is fully removed before return.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::ResourceLimitReached`] when a configured controller's identity-operation budget is full.
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn cancel(&self, id: TaskId) -> Result<bool, RuntimeError> {
        #[cfg(feature = "controller")]
        if let Some(controller) = &self.controller {
            return controller.handle().cancel(id).await;
        }
        self.core().cancel(id).await
    }

    /// Cancels work only if the management queue has capacity now.
    ///
    /// After queue admission, its result and cleanup guarantees match [`cancel`](Self::cancel).
    ///
    /// # Errors
    ///
    /// Returns the errors from [`cancel`](Self::cancel).
    /// It also returns [`RuntimeError::CommandQueueFull`] when a required management queue has no capacity.
    pub async fn try_cancel(&self, id: TaskId) -> Result<bool, RuntimeError> {
        #[cfg(feature = "controller")]
        if let Some(controller) = &self.controller {
            return controller.handle().try_cancel(id).await;
        }
        self.core().try_cancel(id).await
    }

    /// Cancels the registered task with `name` and waits for cleanup.
    ///
    /// Name lookup and the cancellation claim are one registry operation.
    /// The result and terminal guarantees match [`cancel`](Self::cancel).
    /// Controller work that is still queued has no registered name;
    /// cancel it by its returned [`TaskId`].
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn cancel_by_name(&self, name: &str) -> Result<bool, RuntimeError> {
        self.core().cancel_by_name(Arc::from(name)).await
    }

    /// Cancels by name only if the registry queue has capacity now.
    ///
    /// After queue admission, behavior is the same as [`cancel_by_name`](Self::cancel_by_name).
    ///
    /// # Errors
    ///
    /// Returns the errors from [`cancel_by_name`](Self::cancel_by_name).
    /// It also returns [`RuntimeError::CommandQueueFull`] when the registry queue has no capacity.
    pub async fn try_cancel_by_name(&self, name: &str) -> Result<bool, RuntimeError> {
        self.core().try_cancel_by_name(Arc::from(name)).await
    }

    /// Cancels by name and limits how long this caller waits for cleanup.
    ///
    /// Queue admission and the registry claim are outside `wait_for`.
    /// The timer covers only the final wait for task cleanup. A timeout stops waiting.
    /// It does not undo cancellation or change the supervisor grace period.
    ///
    /// The boolean follows [`cancel_by_name`](Self::cancel_by_name).
    /// Queued controller work has no registered name; cancel it by [`TaskId`].
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::TaskTerminationTimeout`] when confirmation does not arrive in time.
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn cancel_by_name_with_timeout(
        &self,
        name: &str,
        wait_for: Duration,
    ) -> Result<bool, RuntimeError> {
        self.core()
            .cancel_by_name_with_timeout(Arc::from(name), wait_for)
            .await
    }

    /// Cancels by name with a wait limit and fail-fast queue admission.
    ///
    /// Fail-fast behavior applies only to queue admission.
    /// Timeout and result behavior match [`cancel_by_name_with_timeout`](Self::cancel_by_name_with_timeout).
    ///
    /// # Errors
    ///
    /// Returns the errors from [`cancel_by_name_with_timeout`](Self::cancel_by_name_with_timeout).
    /// It also returns [`RuntimeError::CommandQueueFull`] when the registry queue has no capacity.
    pub async fn try_cancel_by_name_with_timeout(
        &self,
        name: &str,
        wait_for: Duration,
    ) -> Result<bool, RuntimeError> {
        self.core()
            .try_cancel_by_name_with_timeout(Arc::from(name), wait_for)
            .await
    }

    /// Cancels by identity and limits how long this caller waits for cleanup.
    ///
    /// Controller ordering, queue admission, and the registry claim are outside `wait_for`.
    /// The timer covers only the final wait for registered task cleanup. Queued controller work
    /// is removed directly. This timer does not apply to that path.
    ///
    /// A timeout stops this caller's wait. It does not undo cancellation or change the
    /// supervisor grace period. The boolean follows [`cancel`](Self::cancel).
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::TaskTerminationTimeout`] when confirmation does not arrive in time.
    /// - [`RuntimeError::ResourceLimitReached`] when a configured controller's identity-operation budget is full.
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn cancel_with_timeout(
        &self,
        id: TaskId,
        wait_for: Duration,
    ) -> Result<bool, RuntimeError> {
        #[cfg(feature = "controller")]
        if let Some(controller) = &self.controller {
            return controller.handle().cancel_with_timeout(id, wait_for).await;
        }
        self.core().cancel_with_timeout(id, wait_for).await
    }

    /// Cancels by identity with a wait limit and fail-fast queue admission.
    ///
    /// After queue admission, timeout and result behavior match [`cancel_with_timeout`](Self::cancel_with_timeout).
    ///
    /// # Errors
    ///
    /// Returns the errors from [`cancel_with_timeout`](Self::cancel_with_timeout).
    /// It also returns [`RuntimeError::CommandQueueFull`] when a required management queue has no capacity.
    pub async fn try_cancel_with_timeout(
        &self,
        id: TaskId,
        wait_for: Duration,
    ) -> Result<bool, RuntimeError> {
        #[cfg(feature = "controller")]
        if let Some(controller) = &self.controller {
            return controller
                .handle()
                .try_cancel_with_timeout(id, wait_for)
                .await;
        }
        self.core().try_cancel_with_timeout(id, wait_for).await
    }

    /// Closes runtime admission and waits for the shared bounded cleanup workflow.
    ///
    /// Shutdown closes admission, drains accepted controller work when configured, cancels registered tasks,
    /// waits through the grace window, joins runtime management workers, and drains subscriber queues up to their deadline.
    ///
    /// A force-aborted synchronous task, detached subscriber callback, or isolated user destructor may still be active after return.
    /// Its ownership remains charged until physical release.
    ///
    /// This consumes only the current handle value. Shutdown affects the shared runtime and every clone.
    /// Concurrent or later shutdown calls on other handles receive the same cached result.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::GraceExceeded`] when some tasks did not stop within the grace period.
    /// - [`RuntimeError::SignalSetupFailed`] when this call joins a shutdown started by failed operating-system signal setup.
    /// - [`RuntimeError::ShuttingDown`] when shared runtime cleanup cannot finish normally.
    ///
    /// # Cancel safety
    ///
    /// On its first poll, this method creates or joins a detached shared shutdown operation.
    /// Dropping this caller's future after that point does not stop cleanup.
    #[doc(alias = "graceful shutdown")]
    #[doc(alias = "graceful stop")]
    pub async fn shutdown(self) -> Result<(), RuntimeError> {
        self.core().shutdown().await
    }

    /// Prepares a controller submission and exposes its identity before queue admission.
    ///
    /// This allocates the [`TaskId`] but does not enqueue work or publish an event. The caller can install correlation for
    /// [`PreparedSubmission::id`](crate::PreparedSubmission::id) before consuming the prepared value with a submit method.
    ///
    /// Requires the `controller` feature.
    ///
    /// # Errors
    ///
    /// Returns [`ControllerError::NotConfigured`](crate::ControllerError::NotConfigured)
    /// when this supervisor was built without a controller.
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

    /// Queues work for controller slot admission and returns its reserved [`TaskId`].
    ///
    /// `Ok(id)` confirms only that the controller queue accepted the submission.
    /// Slot admission and runtime registration happen later.
    ///
    /// Requires the `controller` feature.
    ///
    /// # Errors
    ///
    /// - [`ControllerError::NotConfigured`](crate::ControllerError::NotConfigured) when the supervisor has no controller.
    /// - [`ControllerError::ThreadStartFailed`](crate::ControllerError::ThreadStartFailed) when background cleanup workers cannot start.
    /// - [`ControllerError::ResourceLimit`](crate::ControllerError::ResourceLimit) when the configured ownership limit is exhausted.
    /// - [`ControllerError::Closed`](crate::ControllerError::Closed) when the controller has stopped.
    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    pub async fn submit(
        &self,
        spec: crate::controller::ControllerSpec,
    ) -> Result<TaskId, crate::controller::ControllerError> {
        self.prepare_submission(spec)?.submit().await
    }

    /// Queues controller work after a bounded wait for cleanup ownership.
    ///
    /// `wait_for` covers only the ownership permit. After Taskvisor acquires it, the ordinary
    /// wait for controller command capacity has no deadline from this method. An immediately
    /// available permit can succeed when `wait_for` is [`Duration::ZERO`]. The timer cannot
    /// interrupt synchronous lazy cleanup-worker startup.
    ///
    /// A timeout happens before controller command intake and publishes no lifecycle event for
    /// the request.
    ///
    /// Requires the `controller` feature.
    ///
    /// # Errors
    ///
    /// Returns the errors from [`submit`](Self::submit).
    /// It also returns [`ControllerError::OwnershipAdmissionTimeout`](crate::ControllerError::OwnershipAdmissionTimeout)
    /// when ownership remains unavailable for `wait_for`.
    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    pub async fn submit_with_ownership_timeout(
        &self,
        spec: crate::controller::ControllerSpec,
        wait_for: Duration,
    ) -> Result<TaskId, crate::controller::ControllerError> {
        self.prepare_submission(spec)?
            .submit_with_ownership_timeout(wait_for)
            .await
    }

    /// Submits only if the controller queue has capacity now.
    ///
    /// `Ok(id)` has the same queue-only meaning as [`submit`](Self::submit).
    ///
    /// Requires the `controller` feature.
    ///
    /// # Errors
    ///
    /// Returns the errors from [`submit`](Self::submit).
    /// It also returns [`ControllerError::Full`](crate::ControllerError::Full) when the controller queue has no capacity.
    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    pub fn try_submit(
        &self,
        spec: crate::controller::ControllerSpec,
    ) -> Result<TaskId, crate::controller::ControllerError> {
        self.prepare_submission(spec)?.try_submit()
    }

    /// Queues work for controller slot admission and returns a final-outcome waiter.
    ///
    /// The return confirms only controller queue admission.
    /// The waiter receives [`TaskOutcome::Rejected`](crate::TaskOutcome::Rejected) if controller or
    /// registry admission later rejects the work. Admitted work follows the normal [`TaskWaiter`] contract.
    ///
    /// Requires the `controller` feature.
    ///
    /// # Errors
    ///
    /// Returns the same queue-admission errors as [`submit`](Self::submit).
    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    pub async fn submit_and_watch(
        &self,
        spec: crate::controller::ControllerSpec,
    ) -> Result<(TaskId, TaskWaiter), crate::controller::ControllerError> {
        self.prepare_submission(spec)?.submit_and_watch().await
    }

    /// Queues watched controller work after bounded ownership admission.
    ///
    /// Timeout behavior matches [`submit_with_ownership_timeout`](Self::submit_with_ownership_timeout).
    /// After ownership succeeds, intake and outcome behavior match [`submit_and_watch`](Self::submit_and_watch).
    ///
    /// Requires the `controller` feature.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`submit_with_ownership_timeout`](Self::submit_with_ownership_timeout).
    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    pub async fn submit_and_watch_with_ownership_timeout(
        &self,
        spec: crate::controller::ControllerSpec,
        wait_for: Duration,
    ) -> Result<(TaskId, TaskWaiter), crate::controller::ControllerError> {
        self.prepare_submission(spec)?
            .submit_and_watch_with_ownership_timeout(wait_for)
            .await
    }

    /// Submits watched work only if the controller queue has capacity now.
    ///
    /// On success, the waiter behaves like [`submit_and_watch`](Self::submit_and_watch).
    /// `Ok` still confirms only queue admission; slot admission happens later.
    ///
    /// Requires the `controller` feature.
    ///
    /// # Errors
    ///
    /// Returns the errors from [`submit_and_watch`](Self::submit_and_watch).
    /// It also returns [`ControllerError::Full`](crate::ControllerError::Full) when the controller queue has no capacity.
    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    pub fn try_submit_and_watch(
        &self,
        spec: crate::controller::ControllerSpec,
    ) -> Result<(TaskId, TaskWaiter), crate::controller::ControllerError> {
        self.prepare_submission(spec)?.try_submit_and_watch()
    }

    /// Returns a best-effort rolling snapshot of controller slots.
    ///
    /// Slots are copied one at a time. Concurrent changes can appear in only part of one snapshot.
    /// The value can also become stale as soon as this method returns.
    /// It is `None` when this supervisor was built without a controller.
    ///
    /// Requires the `controller` feature.
    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    pub async fn controller_snapshot(&self) -> Option<crate::controller::ControllerSnapshot> {
        match &self.controller {
            Some(ctrl) => Some(ctrl.snapshot().await),
            None => None,
        }
    }
}
