//! # Manage a running supervisor
//!
//! [`Supervisor::serve`](crate::Supervisor::serve) returns a [`SupervisorHandle`].
//!
//! Direct state-changing `add*`, `remove*`, and `cancel*` commands use one bounded management queue.
//! A regular method waits for capacity; its `try_*` form fails fast when that queue is full.
//!
//! With a controller, identity-based remove and cancel methods may use both the controller queue and the registry queue.
//! Regular forms wait at both boundaries, while `try_*` forms fail fast at either one.
//! Controller `submit*` methods confirm queue acceptance only; they do not wait for slot admission.
//!
//! Dropping a management future before its command is accepted may stop the operation.
//! After acceptance, the runtime owns the command and does not roll it back when the caller is dropped.
//!
//! With a controller, identity-based remove and cancel operations are ordered after earlier submissions.
//! This lets them find work that is still queued and has not reached the registry.
//!
//! [`SupervisorCore`]: crate::core::SupervisorCore

use std::{sync::Arc, time::Duration};

use crate::core::{RuntimeOwner, SupervisorCore};
use crate::error::RuntimeError;
use crate::identity::TaskId;
use crate::tasks::TaskSpec;

use super::outcome::TaskWaiter;

/// Cloneable handle for one running supervisor.
///
/// Clones share the same runtime.
/// Dropping one clone does not stop it.
/// Dropping the last public `Supervisor` or handle sends best-effort cancellation, but cannot wait for cleanup.
/// Call [`shutdown`](Self::shutdown) for a confirmed shutdown result.
///
/// > Use [`remove`](Self::remove) to request a stop and return after the request is accepted.
/// > Use [`cancel`](Self::cancel) to wait until the task is joined and its name and identity are released.
///
/// ## Example
///
/// ```rust,no_run
/// use std::time::Duration;
/// use taskvisor::{Supervisor, SupervisorConfig, TaskFn, TaskSpec};
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let supervisor = Supervisor::new(SupervisorConfig::default(), vec![]);
///     let handle = supervisor.serve();
///
///     let task = TaskFn::arc("worker", |ctx| async move {
///         loop {
///             ctx.run_until_cancelled(tokio::time::sleep(Duration::from_secs(1)))
///                 .await?;
///             // Do one unit of work.
///         }
///     });
///
///     let id = handle.add(TaskSpec::restartable(task)).await?;
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

    /// Adds a task and waits until the registry accepts or rejects it.
    ///
    /// `Ok(id)` confirms registration.
    /// It does not mean that the first attempt has started.
    /// This confirmation is direct and does not use the event bus.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    /// - [`RuntimeError::TaskAlreadyExists`] when the task name is already in use.
    pub async fn add(&self, spec: TaskSpec) -> Result<TaskId, RuntimeError> {
        self.core().add_task(spec).await
    }

    /// Adds a task only if the management queue has capacity now.
    ///
    /// After queue admission, it still waits for registry acceptance.
    /// `Ok(id)` has the same meaning as [`add`](Self::add).
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::CommandQueueFull`] when the bounded registry queue has no capacity.
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    /// - [`RuntimeError::TaskAlreadyExists`] when the task name is already in use.
    pub async fn try_add(&self, spec: TaskSpec) -> Result<TaskId, RuntimeError> {
        self.core().try_add_task(spec).await
    }

    /// Adds a task and returns a waiter for its final outcome.
    ///
    /// Registration is the same as [`add`](Self::add).
    /// The [`TaskWaiter`] resolves after all retries end, the registry joins the task actor, and registry membership is removed.
    ///
    /// > It uses a direct completion channel, not best-effort lifecycle events.
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

    /// Adds a watched task only if the management queue has capacity now.
    ///
    /// After queue admission, registration and outcome behavior are the same as [`add_and_watch`](Self::add_and_watch).
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

    /// Requests removal by task identity without waiting for termination.
    ///
    /// `Ok(true)` means this call claimed the task and sent cancellation, or removed it from the controller queue.
    /// `Ok(false)` means the identity was unknown, already finished, or already claimed by another stop request.
    ///
    /// For a registered task, the method returns before final cleanup.
    /// Removing queued controller work is complete when this method returns.
    /// > Use [`cancel`](Self::cancel) when you need to wait until termination.
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

    /// Requests removal only if the management queue has capacity now.
    ///
    /// After queue admission, it waits for the same decision and returns the same boolean as [`remove`](Self::remove).
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

    /// Requests removal of the registered task with `name`.
    ///
    /// Name lookup and the removal claim are one registry operation.
    /// The boolean has the same meaning as [`remove`](Self::remove), and this method also returns before final cleanup.
    ///
    /// Controller submissions that are still queued do not own a registered name.
    /// Remove them with the [`TaskId`] returned by `submit`.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn remove_by_label(&self, name: &str) -> Result<bool, RuntimeError> {
        self.core().remove_by_label(Arc::from(name)).await
    }

    /// Requests removal by name only if the registry queue has capacity now.
    ///
    /// After queue admission, behavior is the same as [`remove_by_label`](Self::remove_by_label).
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::CommandQueueFull`] when the bounded registry queue has no capacity.
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn try_remove_by_label(&self, name: &str) -> Result<bool, RuntimeError> {
        self.core().try_remove_by_label(Arc::from(name)).await
    }

    /// Returns the authoritative registry view as `(id, name)` pairs.
    ///
    /// The list comes from the registry and is sorted by [`TaskId`].
    /// It includes every registry entry, whether its actor is running, waiting for a permit, backoff, or restart interval, already finished but not cleaned up, or being removed.
    ///
    /// See [`alive_snapshot`](Self::alive_snapshot) for the best-effort list of task names currently marked alive.
    pub async fn list(&self) -> Vec<(TaskId, Arc<str>)> {
        self.core().list_tasks().await
    }

    /// Returns task names currently marked alive by lifecycle events.
    ///
    /// This is a best-effort event-derived cache.
    /// It can lag or miss state after event loss.
    /// The result is sorted and deduplicated by name.
    ///
    /// See [`list`](Self::list) for the authoritative registry view of registered tasks.
    pub async fn alive_snapshot(&self) -> Vec<Arc<str>> {
        self.core().snapshot().await
    }

    /// Returns whether the cache currently marks any run with this name as alive.
    ///
    /// This best-effort query does not check a specific [`TaskId`] and can miss state after event loss.
    /// > Use [`list`](Self::list) for registry membership.
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

    /// Cancels work by identity and waits for terminal cleanup.
    ///
    /// For registered work, this returns after the actor is joined and its name and identity are released.
    /// For queued controller work, removal is already complete when this method returns.
    ///
    /// `Ok(true)` is returned only to the call that claimed the stop.
    /// A call that joins an existing removal waits for the same cleanup, then returns `Ok(false)`.
    ///
    /// An unknown or already-cleaned identity returns `Ok(false)` without waiting for task cleanup.
    /// A watched submission removed from a controller queue resolves as [`TaskOutcome::Rejected`](crate::TaskOutcome::Rejected) because its body did not run.
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

    /// Cancels work only if the management queue has capacity now.
    ///
    /// After queue admission, it has the same result and cleanup guarantees as [`cancel`](Self::cancel).
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

    /// Cancels the registered task with `name` and waits for cleanup.
    ///
    /// Name lookup and the cancellation claim are one registry operation.
    /// The result and terminal guarantees match [`cancel`](Self::cancel).
    ///
    /// # Errors
    ///
    /// Same as [`cancel`](Self::cancel).
    /// A controller submission that is still queued has no registered name; cancel it by its returned [`TaskId`].
    pub async fn cancel_by_label(&self, name: &str) -> Result<bool, RuntimeError> {
        self.core().cancel_by_label(Arc::from(name)).await
    }

    /// Cancels by name only if the registry queue has capacity now.
    ///
    /// After queue admission, behavior is the same as [`cancel_by_label`](Self::cancel_by_label).
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::CommandQueueFull`] when the bounded registry queue has no capacity.
    /// - [`RuntimeError::ShuttingDown`] when the runtime no longer accepts commands.
    pub async fn try_cancel_by_label(&self, name: &str) -> Result<bool, RuntimeError> {
        self.core().try_cancel_by_label(Arc::from(name)).await
    }

    /// Cancels by name and limits how long this caller waits for cleanup.
    ///
    /// Queue admission and the registry claim are outside `wait_for`.
    /// The timer covers only the final wait for task cleanup.
    /// A timeout stops waiting but does not undo cancellation or change the supervisor grace period.
    ///
    /// The boolean follows [`cancel_by_label`](Self::cancel_by_label).
    /// Queued controller work has no registered name; cancel it by [`TaskId`].
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

    /// Cancels by name with a wait limit and fail-fast queue admission.
    ///
    /// Fail-fast behavior applies only to queue admission.
    /// The timeout and boolean semantics match [`cancel_by_label_with_timeout`](Self::cancel_by_label_with_timeout).
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

    /// Cancels by identity and limits how long this caller waits for cleanup.
    ///
    /// Controller ordering, queue admission, and the registry claim are outside `wait_for`.
    /// The timer covers only the final wait for registered task cleanup.
    /// Queued controller work is removed directly. This timer does not apply to that path.
    ///
    /// A timeout stops this caller's wait.
    /// It does not undo cancellation or change the supervisor grace period.
    /// The boolean follows [`cancel`](Self::cancel).
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

    /// Cancels by identity with a wait limit and fail-fast queue admission.
    ///
    /// After queue admission, timeout and result behavior match [`cancel_with_timeout`](Self::cancel_with_timeout).
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

    /// Shuts down the runtime and waits for cleanup.
    ///
    /// This closes admission, cancels registered tasks, waits for the configured grace period, and joins internal workers.
    /// Subscriber queues are drained up to their configured deadline.
    /// With a controller, pending submissions are also resolved and the controller worker is joined.
    ///
    /// Shutdown is shared.
    /// Concurrent or later shutdown calls on other handles receive the same cached result.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::GraceExceeded`] when some tasks did not stop within the grace period.
    /// - [`RuntimeError::SignalSetupFailed`] if a concurrent static `run` call started shutdown after signal setup failed.
    /// - [`RuntimeError::ShuttingDown`] when shared runtime cleanup cannot finish normally.
    #[doc(alias = "graceful shutdown")]
    #[doc(alias = "graceful stop")]
    pub async fn shutdown(self) -> Result<(), RuntimeError> {
        self.core().shutdown().await
    }

    /// Sends a task to the controller and returns its reserved [`TaskId`].
    ///
    /// `Ok(id)` confirms only that the controller queue accepted the submission.
    /// Slot admission and runtime registration happen later.
    ///
    /// > Use [`try_submit`](Self::try_submit) to fail fast when the controller queue is full.
    /// > Use [`submit_and_watch`](Self::submit_and_watch) to observe the final outcome, including admission rejection.
    ///
    /// Requires the `controller` feature.
    ///
    /// # Errors
    ///
    /// - [`ControllerError::NotConfigured`](crate::ControllerError::NotConfigured) when the supervisor was built without a controller.
    /// - [`ControllerError::Closed`](crate::ControllerError::Closed) when the controller has stopped.
    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    pub async fn submit(
        &self,
        spec: crate::controller::ControllerSpec,
    ) -> Result<TaskId, crate::controller::ControllerError> {
        match &self.controller {
            Some(ctrl) => ctrl.handle().submit(spec).await,
            None => Err(crate::controller::ControllerError::NotConfigured),
        }
    }

    /// Submits only if the controller queue has capacity now.
    ///
    /// `Ok(id)` has the same queue-only meaning as [`submit`](Self::submit).
    ///
    /// Requires the `controller` feature.
    ///
    /// # Errors
    ///
    /// - [`ControllerError::NotConfigured`](crate::ControllerError::NotConfigured) when the supervisor was built without a controller.
    /// - [`ControllerError::Full`](crate::ControllerError::Full) when the controller queue has no capacity.
    /// - [`ControllerError::Closed`](crate::ControllerError::Closed) when the controller has stopped.
    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    pub fn try_submit(
        &self,
        spec: crate::controller::ControllerSpec,
    ) -> Result<TaskId, crate::controller::ControllerError> {
        match &self.controller {
            Some(ctrl) => ctrl.handle().try_submit(spec),
            None => Err(crate::controller::ControllerError::NotConfigured),
        }
    }

    /// Sends a task to the controller and returns a final-outcome waiter.
    ///
    /// The waiter uses the direct completion channel, not the best-effort event bus.
    /// It normally resolves to [`TaskOutcome::Rejected`](crate::TaskOutcome::Rejected) when controller or registry admission rejects the submission.
    /// If the completion sender closes first, [`TaskWaiter::wait`] returns [`RuntimeError::ShuttingDown`].
    /// If admitted, the waiter resolves after the task finishes and its actor is joined.
    ///
    /// Requires the `controller` feature.
    ///
    /// # Errors
    ///
    /// - [`ControllerError::NotConfigured`](crate::ControllerError::NotConfigured) when the supervisor was built without a controller.
    /// - [`ControllerError::Closed`](crate::ControllerError::Closed) when the controller has stopped.
    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
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

    /// Submits watched work only if the controller queue has capacity now.
    ///
    /// On success, the waiter behaves like [`submit_and_watch`](Self::submit_and_watch).
    /// `Ok` still confirms only queue admission; slot admission happens later.
    ///
    /// Requires the `controller` feature.
    ///
    /// # Errors
    ///
    /// - [`ControllerError::NotConfigured`](crate::ControllerError::NotConfigured) when the supervisor was built without a controller.
    /// - [`ControllerError::Full`](crate::ControllerError::Full) when the controller queue has no capacity.
    /// - [`ControllerError::Closed`](crate::ControllerError::Closed) when the controller has stopped.
    #[cfg(feature = "controller")]
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
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

    /// Returns a best-effort rolling snapshot of controller slots.
    ///
    /// Slots are copied one at a time. Concurrent changes can appear in only part of one snapshot.
    /// The value can also become stale as soon as this method returns.
    /// It is `None` when this supervisor was built without a controller.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use taskvisor::prelude::*;
    /// # #[tokio::main] async fn main() {
    /// # let sup = Supervisor::builder(SupervisorConfig::default())
    /// #     .with_controller(ControllerConfig::default())
    /// #     .build();
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
    #[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
    pub async fn controller_snapshot(&self) -> Option<crate::controller::ControllerSnapshot> {
        match &self.controller {
            Some(ctrl) => Some(ctrl.snapshot().await),
            None => None,
        }
    }
}
