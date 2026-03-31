//! # Supervisor handle for dynamic task management.
//!
//! [`SupervisorHandle`] is returned by [`Supervisor::serve()`] and provides the full runtime management API.
//!
//! This is the **only** way to manage tasks dynamically at runtime.
//! [`Supervisor::run()`] is a self-contained entry point for static task sets.

use std::{sync::Arc, time::Duration};
use tokio::sync::broadcast;

use crate::error::RuntimeError;
use crate::events::EventKind;
use crate::tasks::TaskSpec;

use super::supervisor::Supervisor;

/// Handle for managing a running supervisor.
///
/// Obtained via [`Supervisor::serve()`]. Provides the full runtime management API.
///
/// # Also
///
/// - [`Supervisor`](crate::Supervisor) - the runtime behind this handle
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
///     let task = TaskFn::arc("worker", |ctx: CancellationToken| async move {
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
    inner: Arc<Supervisor>,
}

impl std::fmt::Debug for SupervisorHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupervisorHandle")
            .field("supervisor", &self.inner)
            .finish()
    }
}

impl SupervisorHandle {
    /// Creates a new handle wrapping the given supervisor.
    ///
    /// The supervisor must already be started (via `serve()`).
    pub(crate) fn new(supervisor: Arc<Supervisor>) -> Self {
        Self { inner: supervisor }
    }

    /// Adds a new task to the supervisor at runtime.
    ///
    /// Fire-and-forget: publishes `TaskAddRequested` and returns immediately.
    /// The task will be spawned asynchronously by the registry listener.
    pub fn add(&self, spec: TaskSpec) -> Result<(), RuntimeError> {
        self.inner.add_task(spec)
    }

    /// Adds a task and waits for registration confirmation.
    ///
    /// Subscribes to the event bus **before** publishing `TaskAddRequested`,
    /// then waits for the matching `TaskAdded` event from the registry.
    ///
    /// Returns `Ok(())` when the task is confirmed running, or `RuntimeError::TaskAddTimeout`
    /// if confirmation doesn't arrive within `timeout`.
    pub async fn add_and_wait(
        &self,
        spec: TaskSpec,
        timeout: Duration,
    ) -> Result<(), RuntimeError> {
        let target: Arc<str> = Arc::from(spec.task().name());
        let mut rx = self.inner.subscribe_bus();
        self.inner.add_task(spec)?;

        let target2 = Arc::clone(&target);
        let wait = async move {
            loop {
                match rx.recv().await {
                    Ok(ev)
                        if ev.kind == EventKind::TaskAdded
                            && ev.task.as_deref() == Some(&*target2) =>
                    {
                        return Ok(());
                    }
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if self.inner.registry_contains(&target2).await {
                            return Ok(());
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
            if self.inner.registry_contains(&target2).await {
                Ok(())
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

    /// Removes a task by name.
    ///
    /// Fire-and-forget: publishes `TaskRemoveRequested` and returns immediately.
    /// The task will be cancelled and cleaned up asynchronously by the registry.
    pub fn remove(&self, name: &str) -> Result<(), RuntimeError> {
        self.inner.remove_task(name)
    }

    /// Returns a sorted list of registered task names (from the registry).
    ///
    /// Includes tasks in any state: starting, running, stopping.
    /// See [`snapshot`](Self::snapshot) for only currently executing tasks.
    pub async fn list(&self) -> Vec<Arc<str>> {
        self.inner.list_tasks().await
    }

    /// Returns a sorted list of currently alive task names (from the alive tracker).
    ///
    /// Only includes tasks whose last lifecycle event was [`TaskStarting`](crate::EventKind::TaskStarting).
    /// See [`list`](Self::list) for all registered tasks regardless of state.
    pub async fn snapshot(&self) -> Vec<Arc<str>> {
        self.inner.snapshot().await
    }

    /// Check whether a given task is currently alive.
    pub async fn is_alive(&self, name: &str) -> bool {
        self.inner.is_alive(name).await
    }

    /// Cancel a task by name and wait for confirmation.
    ///
    /// Uses the default grace period from supervisor config.
    pub async fn cancel(&self, name: &str) -> Result<bool, RuntimeError> {
        self.inner.cancel(name).await
    }

    /// Cancel a task with explicit timeout.
    pub async fn cancel_with_timeout(
        &self,
        name: &str,
        wait_for: Duration,
    ) -> Result<bool, RuntimeError> {
        self.inner.cancel_with_timeout(name, wait_for).await
    }

    /// Initiates graceful shutdown: cancels all tasks and waits for them to stop.
    pub async fn shutdown(self) -> Result<(), RuntimeError> {
        self.inner.shutdown().await
    }

    /// Submits a task to the controller (if enabled).
    ///
    /// Requires the `controller` feature flag.
    #[cfg(feature = "controller")]
    pub async fn submit(
        &self,
        spec: crate::controller::ControllerSpec,
    ) -> Result<(), crate::controller::ControllerError> {
        self.inner.submit(spec).await
    }

    /// Tries to submit a task without blocking.
    ///
    /// Returns `ControllerError::Full` if the queue is full.
    ///
    /// Requires the `controller` feature flag.
    #[cfg(feature = "controller")]
    pub fn try_submit(
        &self,
        spec: crate::controller::ControllerSpec,
    ) -> Result<(), crate::controller::ControllerError> {
        self.inner.try_submit(spec)
    }
}
