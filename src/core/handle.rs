//! # Dynamic supervisor handle.
//!
//! [`SupervisorHandle`] is returned by [`Supervisor::serve`](crate::Supervisor::serve).
//! It is the runtime API for adding, removing, cancelling, listing, and shutting down tasks after the supervisor has been started.
//!
//! The handle talks to [`SupervisorCore`] for registered tasks.
//! With the `controller` feature enabled, identity removal and cancellation are first ordered with
//! slot-based submissions so queued work can be removed before it reaches the registry.
//!
//! [`Supervisor::run`](crate::Supervisor::run) is the static entry point for a fixed task set.
//! Use `serve` when tasks must be managed at runtime.
//!
//! [`SupervisorCore`]: crate::core::SupervisorCore

use std::{sync::Arc, time::Duration};

use crate::core::{RuntimeOwner, SupervisorCore};
use crate::error::RuntimeError;
use crate::identity::TaskId;
use crate::tasks::TaskSpec;

use super::outcome::TaskWaiter;

/// Handle for managing a started supervisor.
///
/// A handle is created by [`Supervisor::serve`](crate::Supervisor::serve).
/// It can be cloned and shared between tasks.
/// Dropping one clone does not stop the runtime. Dropping the final public
/// supervisor/handle owner sends non-blocking best-effort cancellation; call
/// [`shutdown`](Self::shutdown) to wait for graceful cleanup and receive its result.
///
/// ## Example
///
/// ```rust,no_run
/// use std::time::Duration;
/// use taskvisor::{
///     Supervisor, SupervisorConfig, TaskContext, TaskError, TaskFn, TaskSpec,
/// };
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
///     let handle = sup.serve();
///
///     let task = TaskFn::arc("worker", |ctx| async move {
///         loop {
///             tokio::select! {
///                 _ = ctx.cancelled() => return Err(TaskError::Canceled),
///                 _ = tokio::time::sleep(Duration::from_secs(1)) => {
///                     // do one unit of work
///                 }
///             }
///         }
///     });
///
///     let id = handle.add(TaskSpec::restartable(task)).await?;
///     let _ = handle.cancel(id).await?;
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

    /// Adds a task and waits for registry acceptance.
    ///
    /// This waits for command queue capacity and then for the direct registry reply.
    /// `Ok(id)` means the registry accepted the task identity and label.
    /// Registration does not depend on lifecycle event delivery.
    ///
    /// Dropping this future after its command was queued does not roll the Add back.
    /// The registry may still accept and start the task.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    /// - [`RuntimeError::TaskAlreadyExists`] when the task name is already in use.
    pub async fn add(&self, spec: TaskSpec) -> Result<TaskId, RuntimeError> {
        self.core().add_task(spec).await
    }

    /// Tries to add a task without waiting for command queue capacity.
    ///
    /// When queue admission succeeds, this still waits for the direct registry reply.
    /// `Ok(id)` therefore has the same meaning as [`add`](Self::add).
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::CommandQueueFull`] when the bounded registry queue has no capacity.
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    /// - [`RuntimeError::TaskAlreadyExists`] when the task name is already in use.
    pub async fn try_add(&self, spec: TaskSpec) -> Result<TaskId, RuntimeError> {
        self.core().try_add_task(spec).await
    }

    /// Adds a task and returns a [`TaskWaiter`] after registry acceptance.
    ///
    /// The waiter resolves to the final [`TaskOutcome`](crate::TaskOutcome) of the supervised task run.
    /// Registration semantics are the same as [`add`](Self::add).
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    /// - [`RuntimeError::TaskAlreadyExists`] when the task name is already in use.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use taskvisor::prelude::*;
    /// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
    /// # let handle = sup.serve();
    /// let job: TaskRef = TaskFn::arc("job", |_ctx| async {
    ///     Ok(())
    /// });
    ///
    /// let (_id, waiter) = handle
    ///     .add_and_watch(TaskSpec::once(job))
    ///     .await?;
    ///
    /// let outcome = waiter.wait().await?;
    /// assert!(outcome.is_success());
    /// # Ok(()) }
    /// ```
    pub async fn add_and_watch(
        &self,
        spec: TaskSpec,
    ) -> Result<(TaskId, TaskWaiter), RuntimeError> {
        let (id, done_rx) = self.core().add_task_watched(spec).await?;
        Ok((id, TaskWaiter::new(id, done_rx)))
    }

    /// Tries to add a watched task without waiting for command queue capacity.
    ///
    /// When queue admission succeeds, this still waits for the direct registry reply before
    /// returning the [`TaskWaiter`]. The registration and completion semantics are otherwise the
    /// same as [`add_and_watch`](Self::add_and_watch).
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::CommandQueueFull`] when the bounded registry queue has no capacity.
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    /// - [`RuntimeError::TaskAlreadyExists`] when the task name is already in use.
    pub async fn try_add_and_watch(
        &self,
        spec: TaskSpec,
    ) -> Result<(TaskId, TaskWaiter), RuntimeError> {
        let (id, done_rx) = self.core().try_add_task_watched(spec).await?;
        Ok((id, TaskWaiter::new(id, done_rx)))
    }

    /// Removes a task by identity.
    ///
    /// With a configured controller, this first orders the id after earlier submissions.
    /// Queued work is removed directly.
    /// Otherwise this waits for registry command capacity and the direct registry reply.
    ///
    /// `Ok(true)` means this call either removed a queued controller submission or claimed a registered task, changed it to `Removing`, and sent cancellation.
    /// `Ok(false)` means the id was unknown, already claimed, or already terminated.
    ///
    /// For registered tasks, this does not wait for terminal cleanup.
    /// Removing queued controller work is complete when this method returns.
    /// Use [`cancel`](Self::cancel) when the call must return after termination.
    ///
    /// With a configured controller, dropping this future before its ordered command is accepted may stop the operation.
    /// Once accepted, the controller owns both queued lookup and registry fallback, so dropping the caller does not undo the operation.
    /// Without a controller, a registry Remove command that was already committed also continues.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn remove(&self, id: TaskId) -> Result<bool, RuntimeError> {
        #[cfg(feature = "controller")]
        if let Some(controller) = &self.controller {
            return controller.handle().remove(id).await;
        }
        self.core().remove(id).await
    }

    /// Tries to remove a task without waiting for command queue capacity.
    ///
    /// When the id is not queued in the controller, successful command admission still waits for the direct registry reply.
    /// Its boolean result has the same meaning as [`remove`](Self::remove).
    /// With a configured controller, this also fails fast when the ordered controller command channel is full.
    /// Once the controller or registry accepts the command, dropping the caller does not undo it.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::CommandQueueFull`] when a bounded controller or registry management queue has no capacity.
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn try_remove(&self, id: TaskId) -> Result<bool, RuntimeError> {
        #[cfg(feature = "controller")]
        if let Some(controller) = &self.controller {
            return controller.handle().try_remove(id).await;
        }
        self.core().try_remove(id).await
    }

    /// Removes the task currently holding `name`.
    ///
    /// Label lookup and the `Registered` to `Removing` transition happen in one registry operation.
    /// `Ok(true)` means this call claimed the label owner and sent cancellation.
    /// `Ok(false)` means there was no registered owner to claim.
    /// This does not wait for terminal task cleanup.
    /// Queued controller submissions are not registered label owners; remove them by the [`TaskId`] returned from `submit` or `submit_and_watch`.
    /// This method waits for registry command capacity. Use
    /// [`try_remove_by_label`](Self::try_remove_by_label) for fail-fast admission.
    /// Once the registry command is committed, dropping the caller does not undo removal.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn remove_by_label(&self, name: &str) -> Result<bool, RuntimeError> {
        self.core().remove_by_label(Arc::from(name)).await
    }

    /// Tries to remove the task currently holding `name` without waiting for registry queue capacity.
    ///
    /// Queued controller submissions are not registered label owners. When command admission
    /// succeeds, this still waits for the authoritative registry reply, and the result has the
    /// same meaning as [`remove_by_label`](Self::remove_by_label). Once the registry command is
    /// committed, dropping the caller does not undo removal.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::CommandQueueFull`] when the bounded registry queue has no capacity.
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn try_remove_by_label(&self, name: &str) -> Result<bool, RuntimeError> {
        self.core().try_remove_by_label(Arc::from(name)).await
    }

    /// Returns registered tasks as `(id, label)` pairs.
    ///
    /// The list comes from the registry and is sorted by [`TaskId`].
    /// It includes both `Registered` and `Removing` tasks.
    ///
    /// See [`alive_snapshot`](Self::alive_snapshot) for the best-effort list of task names currently marked alive.
    pub async fn list(&self) -> Vec<(TaskId, Arc<str>)> {
        self.core().list_tasks().await
    }

    /// Returns a best-effort snapshot of task names currently marked alive.
    ///
    /// This is a best-effort view from the alive tracker, which is fed by the lossy event bus.
    /// The result is sorted and deduplicated by task name.
    ///
    /// See [`list`](Self::list) for the authoritative registry view of registered tasks.
    pub async fn alive_snapshot(&self) -> Vec<Arc<str>> {
        self.core().snapshot().await
    }

    /// Returns a best-effort snapshot of task names currently marked alive.
    ///
    /// This compatibility alias forwards to [`alive_snapshot`](Self::alive_snapshot).
    /// Prefer the explicit name in new code so it is not confused with the authoritative
    /// registry view from [`list`](Self::list).
    pub async fn snapshot(&self) -> Vec<Arc<str>> {
        self.alive_snapshot().await
    }

    /// Returns true if any task run with this name is currently marked alive.
    ///
    /// This is a best-effort label query from the alive tracker.
    /// It does not check task identity.
    pub async fn is_alive(&self, name: &str) -> bool {
        self.core().is_alive(name).await
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

    /// Cancels queued or registered work by identity.
    ///
    /// When the controller claims still-queued work, it removes it directly without a registry
    /// cleanup wait. Registered work waits for terminal registry completion after the actor is
    /// joined and its identity is released.
    /// Returns `Ok(true)` only to the caller that removed a queued controller submission or
    /// claimed registry removal.
    /// A caller that joins an existing removal returns `Ok(false)` after the same terminal completion.
    /// An unknown or terminated id returns `Ok(false)` without waiting for terminal completion.
    /// A watched queued submission resolves to [`TaskOutcome::Rejected`](crate::TaskOutcome::Rejected)
    /// with reason `removed_from_queue` because its task body never started.
    ///
    /// With a configured controller, dropping this future before its ordered command is accepted
    /// may stop the operation. Once accepted, the controller owns both queued lookup and registry
    /// fallback, so dropping the caller does not undo cancellation.
    /// Without a controller, a registry Cancel command that was already committed also continues.
    /// This method waits for controller and registry command capacity. Use
    /// [`try_cancel`](Self::try_cancel) to fail fast when either bounded queue is full.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn cancel(&self, id: TaskId) -> Result<bool, RuntimeError> {
        #[cfg(feature = "controller")]
        if let Some(controller) = &self.controller {
            return controller.handle().cancel(id).await;
        }
        self.core().cancel(id).await
    }

    /// Tries to cancel queued or registered work without waiting for command queue capacity.
    ///
    /// When command admission succeeds, this has the same identity ordering, claimant result,
    /// and terminal-completion semantics as [`cancel`](Self::cancel). With a configured
    /// controller, both the ordered controller queue and the registry fallback are fail-fast.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::CommandQueueFull`] when a bounded controller or registry management queue has no capacity.
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn try_cancel(&self, id: TaskId) -> Result<bool, RuntimeError> {
        #[cfg(feature = "controller")]
        if let Some(controller) = &self.controller {
            return controller.handle().try_cancel(id).await;
        }
        self.core().try_cancel(id).await
    }

    /// Cancels the task currently holding `name`.
    ///
    /// Label lookup and the cancellation claim happen atomically in the registry. This waits for
    /// registry command capacity, the authoritative claim decision, and terminal completion.
    /// Once the registry command is committed, dropping the caller does not undo cancellation.
    /// Use [`try_cancel_by_label`](Self::try_cancel_by_label) for fail-fast admission.
    ///
    /// # Errors
    ///
    /// Same as [`cancel`](Self::cancel).
    /// Queued controller submissions are not registered label owners; cancel them by their
    /// returned [`TaskId`].
    pub async fn cancel_by_label(&self, name: &str) -> Result<bool, RuntimeError> {
        self.core().cancel_by_label(Arc::from(name)).await
    }

    /// Tries to cancel the task currently holding `name` without waiting for registry queue capacity.
    ///
    /// Queued controller submissions are not registered label owners. When command admission
    /// succeeds, this still waits for the authoritative registry decision and terminal
    /// completion. The result has the same meaning as [`cancel_by_label`](Self::cancel_by_label),
    /// and dropping the caller does not undo a committed registry cancellation.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::CommandQueueFull`] when the bounded registry queue has no capacity.
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn try_cancel_by_label(&self, name: &str) -> Result<bool, RuntimeError> {
        self.core().try_cancel_by_label(Arc::from(name)).await
    }

    /// Cancels the task currently holding `name` with an explicit confirmation window.
    ///
    /// Queued controller submissions are not registered label owners; cancel them by their
    /// returned [`TaskId`]. Registry queue admission and the atomic label lookup/claim decision
    /// are outside `wait_for`. The timer starts only while this caller waits for shared terminal
    /// completion. A timeout does not stop removal, and dropping the caller does not undo a
    /// committed registry cancellation.
    ///
    /// This method waits for registry command capacity. Use
    /// [`try_cancel_by_label_with_timeout`](Self::try_cancel_by_label_with_timeout) for fail-fast
    /// admission.
    ///
    /// On completion, the boolean follows the same claimant rules as
    /// [`cancel_by_label`](Self::cancel_by_label).
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::TaskTerminationTimeout`] when confirmation does not arrive in time.
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn cancel_by_label_with_timeout(
        &self,
        name: &str,
        wait_for: Duration,
    ) -> Result<bool, RuntimeError> {
        self.core()
            .cancel_by_label_with_timeout(Arc::from(name), wait_for)
            .await
    }

    /// Tries to cancel the task currently holding `name` with an explicit confirmation window,
    /// without waiting for registry queue capacity.
    ///
    /// Fail-fast behavior applies only to queue admission. Once admitted, this still waits for
    /// the authoritative registry decision. That decision is outside `wait_for`; the timer only
    /// bounds shared terminal completion. Queued controller submissions are not registered label
    /// owners, and a timeout or dropped caller does not undo a committed cancellation.
    ///
    /// On completion, the boolean follows the same claimant rules as
    /// [`cancel_by_label`](Self::cancel_by_label).
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::TaskTerminationTimeout`] when confirmation does not arrive in time.
    /// - [`RuntimeError::CommandQueueFull`] when the bounded registry queue has no capacity.
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn try_cancel_by_label_with_timeout(
        &self,
        name: &str,
        wait_for: Duration,
    ) -> Result<bool, RuntimeError> {
        self.core()
            .try_cancel_by_label_with_timeout(Arc::from(name), wait_for)
            .await
    }

    /// Cancels a task with an explicit confirmation window.
    ///
    /// When the controller claims still-queued work, it removes it directly, so `wait_for` does
    /// not apply to that path.
    /// Controller ordering and the registry claim/join decision are not part of `wait_for`.
    /// The timer starts only while this caller waits for shared terminal completion.
    /// A timeout does not stop removal or change the registry's force-abort grace period.
    /// Once the ordered controller command is accepted, dropping this future does not stop its
    /// queued lookup or registry fallback.
    /// Without a controller, dropping the caller does not undo a committed registry cancellation.
    /// This method waits for controller and registry command capacity. Use
    /// [`try_cancel_with_timeout`](Self::try_cancel_with_timeout) to fail fast when either bounded
    /// queue is full.
    ///
    /// On completion, the boolean follows the same claimant rules as [`cancel`](Self::cancel).
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::TaskTerminationTimeout`] when confirmation does not arrive in time.
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

    /// Tries to cancel a task with an explicit terminal-confirmation window without waiting for
    /// command queue capacity.
    ///
    /// Command admission and the registry claim decision are outside `wait_for`, just as in
    /// [`cancel_with_timeout`](Self::cancel_with_timeout). With a configured controller, both the
    /// ordered controller queue and the registry fallback are fail-fast.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::TaskTerminationTimeout`] when confirmation does not arrive in time.
    /// - [`RuntimeError::CommandQueueFull`] when a bounded controller or registry management queue has no capacity.
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
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

    /// Initiates graceful shutdown of the supervisor runtime.
    ///
    /// With the `controller` feature enabled, this returns only after the controller command
    /// channel is closed, pending controller work is resolved, and the controller loop is joined.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::GraceExceeded`] when some tasks did not stop within the grace period.
    pub async fn shutdown(self) -> Result<(), RuntimeError> {
        self.core().shutdown().await
    }

    /// Submits a task to the controller and returns its pre-minted [`TaskId`].
    ///
    /// This waits for controller queue capacity when needed.
    /// `Ok(id)` means the submission was queued for controller processing, not that it was admitted to a slot or registered in the core runtime.
    ///
    /// Use [`try_submit`](Self::try_submit) to fail fast when the controller queue is full.
    /// Use [`submit_and_watch`](Self::submit_and_watch) to observe the final outcome, including admission rejection.
    ///
    /// Requires the `controller` feature.
    ///
    /// # Errors
    ///
    /// - [`ControllerError::NotConfigured`](crate::ControllerError::NotConfigured) when the supervisor was built without a controller.
    /// - [`ControllerError::Closed`](crate::ControllerError::Closed) when the controller has stopped.
    #[cfg(feature = "controller")]
    pub async fn submit(
        &self,
        spec: crate::controller::ControllerSpec,
    ) -> Result<TaskId, crate::controller::ControllerError> {
        match &self.controller {
            Some(ctrl) => ctrl.handle().submit(spec).await,
            None => Err(crate::controller::ControllerError::NotConfigured),
        }
    }

    /// Tries to submit a task to the controller without waiting.
    ///
    /// `Ok(id)` has the same queued-not-admitted meaning as [`submit`](Self::submit).
    ///
    /// Requires the `controller` feature.
    ///
    /// # Errors
    ///
    /// - [`ControllerError::NotConfigured`](crate::ControllerError::NotConfigured) when the supervisor was built without a controller.
    /// - [`ControllerError::Full`](crate::ControllerError::Full) when the controller queue has no capacity.
    /// - [`ControllerError::Closed`](crate::ControllerError::Closed) when the controller has stopped.
    #[cfg(feature = "controller")]
    pub fn try_submit(
        &self,
        spec: crate::controller::ControllerSpec,
    ) -> Result<TaskId, crate::controller::ControllerError> {
        match &self.controller {
            Some(ctrl) => ctrl.handle().try_submit(spec),
            None => Err(crate::controller::ControllerError::NotConfigured),
        }
    }

    /// Submits a task to the controller and returns a [`TaskWaiter`].
    ///
    /// This is the controller-path version of [`add_and_watch`](Self::add_and_watch).
    /// The waiter resolves on the guaranteed completion plane, not through the lossy event bus.
    ///
    /// If the controller never admits the submission, the waiter resolves to [`TaskOutcome::Rejected`](crate::TaskOutcome::Rejected).
    /// If the submission is admitted, the waiter resolves like [`add_and_watch`](Self::add_and_watch) after the task fully terminates.
    ///
    /// Requires the `controller` feature.
    ///
    /// # Errors
    ///
    /// - [`ControllerError::NotConfigured`](crate::ControllerError::NotConfigured) when the supervisor was built without a controller.
    /// - [`ControllerError::Closed`](crate::ControllerError::Closed) when the controller has stopped.
    #[cfg(feature = "controller")]
    pub async fn submit_and_watch(
        &self,
        spec: crate::controller::ControllerSpec,
    ) -> Result<(TaskId, TaskWaiter), crate::controller::ControllerError> {
        match &self.controller {
            Some(ctrl) => {
                let (id, rx) = ctrl.handle().submit_and_watch(spec).await?;
                Ok((id, TaskWaiter::new(id, rx)))
            }
            None => Err(crate::controller::ControllerError::NotConfigured),
        }
    }

    /// Tries to submit a watched task to the controller without waiting for command queue capacity.
    ///
    /// On success, the returned [`TaskWaiter`] has the same final-outcome semantics as
    /// [`submit_and_watch`](Self::submit_and_watch). `Ok` means only that the ordered controller
    /// channel accepted the submission; slot admission happens later.
    ///
    /// Requires the `controller` feature.
    ///
    /// # Errors
    ///
    /// - [`ControllerError::NotConfigured`](crate::ControllerError::NotConfigured) when the supervisor was built without a controller.
    /// - [`ControllerError::Full`](crate::ControllerError::Full) when the controller queue has no capacity.
    /// - [`ControllerError::Closed`](crate::ControllerError::Closed) when the controller has stopped.
    #[cfg(feature = "controller")]
    pub fn try_submit_and_watch(
        &self,
        spec: crate::controller::ControllerSpec,
    ) -> Result<(TaskId, TaskWaiter), crate::controller::ControllerError> {
        match &self.controller {
            Some(ctrl) => {
                let (id, rx) = ctrl.handle().try_submit_and_watch(spec)?;
                Ok((id, TaskWaiter::new(id, rx)))
            }
            None => Err(crate::controller::ControllerError::NotConfigured),
        }
    }

    /// Returns a point-in-time snapshot of controller slots.
    ///
    /// Returns `None` when the controller feature is enabled but this supervisor was built without a controller.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use taskvisor::prelude::*;
    /// # #[tokio::main] async fn main() {
    /// # let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
    /// # let handle = sup.serve();
    /// if let Some(snap) = handle.controller_snapshot().await {
    ///     println!("{} running, {} queued", snap.running_count(), snap.total_queued());
    ///
    ///     if let Some(web) = snap.slot("web") {
    ///         println!("web: {:?}, depth {}", web.status, web.queue_depth);
    ///     }
    /// }
    /// # }
    /// ```
    ///
    /// Requires the `controller` feature.
    #[cfg(feature = "controller")]
    pub async fn controller_snapshot(&self) -> Option<crate::controller::ControllerSnapshot> {
        match &self.controller {
            Some(ctrl) => Some(ctrl.snapshot().await),
            None => None,
        }
    }
}
