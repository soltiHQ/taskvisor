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
    /// This is fire-and-forget.
    /// It reserves command queue capacity, publishes `TaskRemoveRequested`, queues a remove command, and returns.
    ///
    /// `Ok(())` does not mean the task existed or has already stopped.
    /// Unknown ids are handled by the registry as no-op removals.
    ///
    /// Use [`cancel`](Self::cancel) when you need to wait for `TaskRemoved`, or [`remove_by_label`](Self::remove_by_label) when you only have a task name.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::CommandQueueFull`] when the bounded registry queue has no capacity.
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub fn remove(&self, id: TaskId) -> Result<(), RuntimeError> {
        self.core.remove(id)
    }

    /// Removes the task currently holding `name`.
    ///
    /// Returns `Ok(true)` when a task with this label was found and the remove command was queued.
    /// Returns `Ok(false)` when no registered task currently has this label.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::CommandQueueFull`] when the bounded registry queue has no capacity.
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn remove_by_label(&self, name: &str) -> Result<bool, RuntimeError> {
        match self.core.id_for_label(name).await {
            Some(id) => {
                self.core.remove(id)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Returns registered tasks as `(id, label)` pairs.
    ///
    /// The list comes from the registry and is sorted by [`TaskId`].
    /// It includes registered tasks in any lifecycle state, including starting, running, and stopping.
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

    /// Requests cancellation of a task and waits for removal confirmation.
    ///
    /// The task receives cooperative cancellation.
    /// The method waits up to the configured shutdown grace period for the matching `TaskRemoved` event.
    ///
    /// Returns `Ok(true)` when the task was present and removal was confirmed.
    /// Returns `Ok(false)` when no registered task has this id.
    ///
    /// A task that ignores cancellation is force-aborted by the registry after the configured grace period.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::TaskRemoveTimeout`] when removal was not confirmed in time.
    /// - [`RuntimeError::CommandQueueFull`] when the bounded registry queue has no capacity.
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn cancel(&self, id: TaskId) -> Result<bool, RuntimeError> {
        self.core.cancel(id).await
    }

    /// Cancels the task currently holding `name`.
    ///
    /// Returns `Ok(false)` if no registered task currently has this label.
    /// Otherwise, behaves like [`cancel`](Self::cancel).
    ///
    /// # Errors
    ///
    /// Same as [`cancel`](Self::cancel).
    pub async fn cancel_by_label(&self, name: &str) -> Result<bool, RuntimeError> {
        match self.core.id_for_label(name).await {
            Some(id) => self.core.cancel(id).await,
            None => Ok(false),
        }
    }

    /// Cancels a task with an explicit confirmation window.
    ///
    /// `wait_for` controls how long this call waits for `TaskRemoved`.
    /// It does not change the registry's force-abort grace period.
    ///
    /// Returns `Ok(true)` when removal is confirmed within `wait_for`.
    /// Returns `Ok(false)` when no registered task has this id.
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
    /// Shutdown cancels all registered tasks, waits up to the configured grace period, force-aborts tasks that do not stop,
    /// joins internal listeners, and allows subscriber workers to drain until their separate shutdown timeout.
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
