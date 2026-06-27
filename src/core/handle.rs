//! # Supervisor handle for dynamic task management.
//!
//! [`SupervisorHandle`] is returned by [`Supervisor::serve()`](crate::Supervisor::serve) and provides
//! the full runtime management API. It holds the runtime ([`SupervisorCore`]) directly and, when the
//! `controller` feature is enabled, the optional controller — delegating to each without going through
//! the [`Supervisor`](crate::Supervisor) facade.
//!
//! This is the **only** way to manage tasks dynamically at runtime.
//! [`Supervisor::run()`](crate::Supervisor::run) is a self-contained entry point for static task sets.
//!
//! [`SupervisorCore`]: crate::core::SupervisorCore

use std::{sync::Arc, time::Duration};
use tokio::sync::broadcast;

use crate::core::SupervisorCore;
use crate::error::RuntimeError;
use crate::events::EventKind;
use crate::identity::TaskId;
use crate::tasks::TaskSpec;

use super::outcome::TaskWaiter;

/// Handle for managing a running supervisor.
///
/// Obtained via [`Supervisor::serve()`](crate::Supervisor::serve). Provides the full runtime management API.
///
/// # Also
///
/// - [`Supervisor`](crate::Supervisor) - the facade that produces this handle
/// - [`SupervisorConfig`](crate::SupervisorConfig) - configuration knobs
/// - [`TaskSpec`](crate::TaskSpec) - task configuration passed to [`add`](Self::add)
///
/// ## Example
/// ```rust,no_run
/// use taskvisor::prelude::*;
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
///     let handle = sup.serve();
///
///     let task = TaskFn::arc("worker", |ctx: TaskContext| async move {
///         while !ctx.is_cancelled() {
///             tokio::time::sleep(std::time::Duration::from_secs(1)).await;
///         }
///         Ok(())
///     });
///     handle.add(TaskSpec::restartable(task))?;
///
///     // ... later ...
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
    /// Creates a new handle over the (already-started) runtime.
    pub(crate) fn new(core: Arc<SupervisorCore>) -> Self {
        Self {
            core,
            #[cfg(feature = "controller")]
            controller: None,
        }
    }

    /// Attaches the optional controller (builder/facade wiring).
    #[cfg(feature = "controller")]
    pub(crate) fn with_controller(
        mut self,
        controller: Option<Arc<crate::controller::Controller>>,
    ) -> Self {
        self.controller = controller;
        self
    }

    /// Adds a new task to the supervisor at runtime.
    ///
    /// Fire-and-forget: mints the [`TaskId`], publishes `TaskAddRequested`, and returns immediately with the id.
    /// The task is spawned asynchronously by the registry listener.
    pub fn add(&self, spec: TaskSpec) -> Result<TaskId, RuntimeError> {
        self.core.add_task(spec)
    }

    /// Adds a task and waits for registration confirmation.
    ///
    /// Subscribes to the event bus **before** publishing `TaskAddRequested`,
    /// then waits for the matching `TaskAdded`/`TaskAddFailed` event from the registry.
    ///
    /// Returns:
    /// - `Ok(TaskId)` when the task is confirmed running,
    /// - `Err(RuntimeError::TaskAlreadyExists)` if a task with the same name is already registered,
    /// - `Err(RuntimeError::TaskAddTimeout)` if no confirmation arrives within `timeout`.
    ///
    /// Correlation is by the minted [`TaskId`] (the canonical key), not the task name.
    pub async fn add_and_wait(
        &self,
        spec: TaskSpec,
        timeout: Duration,
    ) -> Result<TaskId, RuntimeError> {
        let target: Arc<str> = Arc::from(spec.task().name());
        let mut rx = self.core.subscribe_bus();
        let id = self.core.add_task(spec)?;
        self.wait_registered(&mut rx, id, target, timeout).await
    }

    /// Adds a task, waits for registration confirmation, and returns an awaitable [`TaskWaiter`] resolving to the task's final [`TaskOutcome`](crate::TaskOutcome).
    ///
    /// The outcome channel is created **atomically with registration** (it travels with the `Add` command over the guaranteed-delivery mpsc).
    /// There is no window in which the task can finish before the waiter exists, and the outcome cannot be lost to event-bus lag.
    ///
    /// Registration semantics are identical to [`add_and_wait`](Self::add_and_wait):
    ///
    /// - `Ok((TaskId, TaskWaiter))` when the task is confirmed running,
    /// - `Err(RuntimeError::TaskAlreadyExists)` if the name is already registered,
    /// - `Err(RuntimeError::TaskAddTimeout)` if no confirmation arrives within `timeout`.
    ///
    /// ## Example
    /// ```rust,no_run
    /// # use std::time::Duration;
    /// # use taskvisor::prelude::*;
    /// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
    /// # let handle = sup.serve();
    /// let job: TaskRef = TaskFn::arc("job", |_ctx: TaskContext| async { Ok(()) });
    /// let (_id, waiter) = handle
    ///     .add_and_watch(TaskSpec::once(job), Duration::from_secs(1))
    ///     .await?;
    ///
    /// let outcome = waiter.wait().await?;
    /// assert!(outcome.is_success());
    /// # Ok(()) }
    /// ```
    pub async fn add_and_watch(
        &self,
        spec: TaskSpec,
        timeout: Duration,
    ) -> Result<(TaskId, TaskWaiter), RuntimeError> {
        let target: Arc<str> = Arc::from(spec.task().name());
        let mut rx = self.core.subscribe_bus();
        let (id, done_rx) = self.core.add_task_watched(spec)?;
        self.wait_registered(&mut rx, id, target, timeout).await?;
        Ok((id, TaskWaiter::new(id, done_rx)))
    }

    /// Waits for the registry's `TaskAdded`/`TaskAddFailed` confirmation for `id`.
    ///
    /// On bus `Lagged`/`Closed` falls back to the registry state (`contains_id`) instead of failing spuriously.
    async fn wait_registered(
        &self,
        rx: &mut broadcast::Receiver<Arc<crate::Event>>,
        id: TaskId,
        target: Arc<str>,
        timeout: Duration,
    ) -> Result<TaskId, RuntimeError> {
        let target2 = Arc::clone(&target);
        let wait = async move {
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.id == Some(id) && ev.kind == EventKind::TaskAdded => {
                        return Ok(id);
                    }
                    Ok(ev) if ev.id == Some(id) && ev.kind == EventKind::TaskAddFailed => {
                        return Err(RuntimeError::TaskAlreadyExists { name: target2 });
                    }
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if self.core.contains_id(id).await {
                            return Ok(id);
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
            if self.core.contains_id(id).await {
                Ok(id)
            } else {
                Err(RuntimeError::TaskAddTimeout {
                    name: target2,
                    timeout,
                })
            }
        };

        match tokio::time::timeout(timeout, wait).await {
            Ok(result) => result,
            Err(_) => Err(RuntimeError::TaskAddTimeout {
                name: target,
                timeout,
            }),
        }
    }

    /// Removes a task by identity.
    ///
    /// Fire-and-forget: publishes `TaskRemoveRequested` and returns immediately.
    ///
    /// The task is cancelled and cleaned up asynchronously by the registry;
    /// a task that ignores cancellation is force-aborted after the configured grace period.
    pub fn remove(&self, id: TaskId) -> Result<(), RuntimeError> {
        self.core.remove(id)
    }

    /// Removes the task currently holding `name` (label); `false` if no such task.
    pub async fn remove_by_label(&self, name: &str) -> Result<bool, RuntimeError> {
        match self.core.id_for_label(name).await {
            Some(id) => {
                self.core.remove(id)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Returns a sorted list of registered task names (from the registry).
    ///
    /// Includes tasks in any state: starting, running, stopping.
    /// See [`snapshot`](Self::snapshot) for only currently executing tasks.
    pub async fn list(&self) -> Vec<(TaskId, Arc<str>)> {
        self.core.list_tasks().await
    }

    /// Returns a sorted list of currently alive task names (from the alive tracker).
    ///
    /// Only includes tasks whose last lifecycle event was [`TaskStarting`](crate::EventKind::TaskStarting).
    /// See [`list`](Self::list) for all registered tasks regardless of state.
    pub async fn snapshot(&self) -> Vec<Arc<str>> {
        self.core.snapshot().await
    }

    /// Check whether a given task is currently alive.
    pub async fn is_alive(&self, name: &str) -> bool {
        self.core.is_alive(name).await
    }

    /// Requests cooperative cancellation of a task and waits up to the configured grace period for it to actually stop (its `TaskRemoved` event).
    ///
    /// A task that ignores cancellation is **force-aborted after the grace period**;
    pub async fn cancel(&self, id: TaskId) -> Result<bool, RuntimeError> {
        self.core.cancel(id).await
    }

    /// Cancel the task currently holding `name` (label), resolving to its identity.
    ///
    /// Returns `Ok(false)` if no task currently holds that label;
    /// otherwise behaves like [`cancel`](Self::cancel) (including the `TaskRemoveTimeout` case for a task that won't stop).
    pub async fn cancel_by_label(&self, name: &str) -> Result<bool, RuntimeError> {
        match self.core.id_for_label(name).await {
            Some(id) => self.core.cancel(id).await,
            None => Ok(false),
        }
    }

    /// Cancel a task with an explicit confirmation window `wait_for` (instead of the configured grace).
    ///
    /// Returns `Ok(true)` once the task confirms termination within `wait_for`, `Ok(false)`
    /// if no such task is registered, or `Err(RuntimeError::TaskRemoveTimeout)`.
    ///
    /// Regardless of `wait_for`, a task that ignores cancellation is still force-aborted after the supervisor's grace period.
    pub async fn cancel_with_timeout(
        &self,
        id: TaskId,
        wait_for: Duration,
    ) -> Result<bool, RuntimeError> {
        self.core.cancel_with_timeout(id, wait_for).await
    }

    /// Initiates graceful shutdown: cancels all tasks and waits for them to stop.
    ///
    /// The controller (if any) winds down via the shared runtime cancellation token.
    pub async fn shutdown(self) -> Result<(), RuntimeError> {
        self.core.shutdown().await
    }

    /// Submits a task to the controller (if enabled), returning its pre-minted [`TaskId`].
    ///
    /// Fire-and-forget: `Ok(id)` means the submission was enqueued, not admitted.
    /// To **await the final outcome** (including admission rejection), use [`submit_and_watch`](Self::submit_and_watch).
    ///
    /// Requires the `controller` feature flag.
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

    /// Tries to submit a task without blocking, returning its pre-minted [`TaskId`].
    ///
    /// Returns `ControllerError::Full` if the queue is full.
    /// See [`submit`](Self::submit) for the enqueued-vs-admitted semantics.
    ///
    /// Requires the `controller` feature flag.
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

    /// Submits a task to the controller and returns an awaitable [`TaskWaiter`] resolving to its final [`TaskOutcome`](crate::TaskOutcome)
    /// the controller-path analogue of [`add_and_watch`](Self::add_and_watch).
    ///
    /// The outcome is delivered on the guaranteed completion plane (a `oneshot`, immune to bus lag).
    /// If the controller never admits the submission (slot busy under `DropIfRunning`, queue full, superseded by a later `Replace`, removed while queued, or shutting down)
    /// the waiter resolves to [`TaskOutcome::Rejected`](crate::TaskOutcome::Rejected) - the task body never ran.
    ///
    /// Otherwise, it resolves like [`add_and_watch`](Self::add_and_watch) once the admitted task fully terminates.
    ///
    /// Requires the `controller` feature flag.
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

    /// Returns a point-in-time [`ControllerSnapshot`](crate::ControllerSnapshot) of the controller's slots, or `None` if no controller is configured.
    ///
    /// ## Example
    /// ```rust,no_run
    /// # use taskvisor::prelude::*;
    /// # #[tokio::main] async fn main() {
    /// # let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
    /// # let handle = sup.serve();
    /// if let Some(snap) = handle.controller_snapshot().await {
    ///     println!("{} running, {} queued", snap.running_count(), snap.total_queued());
    ///     if let Some(web) = snap.slot("web") {
    ///         println!("web: {:?}, depth {}", web.status, web.queue_depth);
    ///     }
    /// }
    /// # }
    /// ```
    ///
    /// Requires the `controller` feature flag.
    #[cfg(feature = "controller")]
    pub async fn controller_snapshot(&self) -> Option<crate::controller::ControllerSnapshot> {
        match &self.controller {
            Some(ctrl) => Some(ctrl.snapshot().await),
            None => None,
        }
    }
}
