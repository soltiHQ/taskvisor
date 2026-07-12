//! # Dynamic supervisor handle.
//!
//! [`SupervisorHandle`] is returned by [`Supervisor::serve`](crate::Supervisor::serve).
//! It is the runtime API for adding, removing, cancelling, listing, and shutting down tasks after the supervisor has been started.
//!
//! The handle talks directly to [`SupervisorCore`].
//! With the `controller` feature enabled, it may also hold a controller handle for slot-based submissions.
//!
//! [`Supervisor::run`](crate::Supervisor::run) is the static entry point for a fixed task set.
//! Use `serve` when tasks must be managed at runtime.
//!
//! [`SupervisorCore`]: crate::core::SupervisorCore

use std::{sync::Arc, time::Duration};

use crate::core::SupervisorCore;
use crate::error::RuntimeError;
use crate::identity::TaskId;
use crate::tasks::TaskSpec;

use super::outcome::TaskWaiter;

/// Handle for managing a started supervisor.
///
/// A handle is created by [`Supervisor::serve`](crate::Supervisor::serve).
/// It can be cloned and shared between tasks.
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
    core: Arc<SupervisorCore>,

    #[cfg(feature = "controller")]
    controller: Option<Arc<crate::controller::Controller>>,
}

impl std::fmt::Debug for SupervisorHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupervisorHandle")
            .field("core", &self.core)
            .finish_non_exhaustive()
    }
}

impl SupervisorHandle {
    /// Creates a new handle over an already-started runtime core.
    pub(crate) fn new(core: Arc<SupervisorCore>) -> Self {
        Self {
            core,
            #[cfg(feature = "controller")]
            controller: None,
        }
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
        self.core.add_task(spec).await
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
        self.core.try_add_task(spec).await
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
        let (id, done_rx) = self.core.add_task_watched(spec).await?;
        Ok((id, TaskWaiter::new(id, done_rx)))
    }

    /// Removes a task by identity.
    ///
    /// This waits for command queue capacity and then for the direct registry reply.
    /// `Ok(true)` means this call claimed the task, changed it to `Removing`, and sent cancellation.
    /// `Ok(false)` means the task was unknown, already removing, or already terminated.
    ///
    /// This does not wait for terminal task cleanup.
    /// Use [`cancel`](Self::cancel) when the call must return after termination.
    ///
    /// Dropping this future after its command was queued does not roll the Remove back.
    /// The registry may still claim and remove the task.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn remove(&self, id: TaskId) -> Result<bool, RuntimeError> {
        self.core.remove(id).await
    }

    /// Tries to remove a task without waiting for command queue capacity.
    ///
    /// When queue admission succeeds, this still waits for the direct registry reply.
    /// Its boolean result has the same meaning as [`remove`](Self::remove).
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::CommandQueueFull`] when the bounded registry queue has no capacity.
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn try_remove(&self, id: TaskId) -> Result<bool, RuntimeError> {
        self.core.try_remove(id).await
    }

    /// Removes the task currently holding `name`.
    ///
    /// Label lookup and the `Registered` to `Removing` transition happen in one registry operation.
    /// `Ok(true)` means this call claimed the label owner and sent cancellation.
    /// `Ok(false)` means there was no registered owner to claim.
    /// This does not wait for terminal task cleanup.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn remove_by_label(&self, name: &str) -> Result<bool, RuntimeError> {
        self.core.remove_by_label(Arc::from(name)).await
    }

    /// Returns registered tasks as `(id, label)` pairs.
    ///
    /// The list comes from the registry and is sorted by [`TaskId`].
    /// It includes both `Registered` and `Removing` tasks.
    ///
    /// See [`snapshot`](Self::snapshot) for the best-effort list of task names currently marked alive.
    pub async fn list(&self) -> Vec<(TaskId, Arc<str>)> {
        self.core.list_tasks().await
    }

    /// Returns task names currently marked alive.
    ///
    /// This is a best-effort view from the alive tracker, which is fed by the lossy event bus.
    /// The result is sorted and deduplicated by task name.
    ///
    /// See [`list`](Self::list) for the authoritative registry view of registered tasks.
    pub async fn snapshot(&self) -> Vec<Arc<str>> {
        self.core.snapshot().await
    }

    /// Returns true if any task run with this name is currently marked alive.
    ///
    /// This is a best-effort label query from the alive tracker.
    /// It does not check task identity.
    pub async fn is_alive(&self, name: &str) -> bool {
        self.core.is_alive(name).await
    }

    /// Requests cancellation and waits for registry terminal completion.
    ///
    /// Returns `Ok(true)` only to the caller that claimed removal.
    /// A caller that joins an existing removal returns `Ok(false)` after the same terminal completion.
    /// An unknown or terminated id returns `Ok(false)` immediately.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::CommandQueueFull`] when the bounded registry queue has no capacity.
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn cancel(&self, id: TaskId) -> Result<bool, RuntimeError> {
        self.core.cancel(id).await
    }

    /// Cancels the task currently holding `name`.
    ///
    /// # Errors
    ///
    /// Same as [`cancel`](Self::cancel).
    pub async fn cancel_by_label(&self, name: &str) -> Result<bool, RuntimeError> {
        self.core.cancel_by_label(Arc::from(name)).await
    }

    /// Cancels a task with an explicit confirmation window.
    ///
    /// The registry claim/join decision is not part of `wait_for`.
    /// The timer starts only while this caller waits for shared terminal completion.
    /// A timeout does not stop removal or change the registry's force-abort grace period.
    ///
    /// On completion, the boolean follows the same claimant rules as [`cancel`](Self::cancel).
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::TaskRemoveTimeout`] when confirmation does not arrive in time.
    /// - [`RuntimeError::CommandQueueFull`] when the bounded registry queue has no capacity.
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn cancel_with_timeout(
        &self,
        id: TaskId,
        wait_for: Duration,
    ) -> Result<bool, RuntimeError> {
        self.core.cancel_with_timeout(id, wait_for).await
    }

    /// Initiates graceful shutdown of the supervisor runtime.
    ///
    /// With the `controller` feature enabled, the controller stops through the shared runtime cancellation token.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::GraceExceeded`] when some tasks did not stop within the grace period.
    pub async fn shutdown(self) -> Result<(), RuntimeError> {
        self.core.shutdown().await
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
