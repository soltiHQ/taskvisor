//! # Supervisor handle for dynamic task management.
//!
//! [`SupervisorHandle`] is returned by [`Supervisor::serve()`] and provides
//! the full runtime management API: add/remove/cancel tasks, query state,
//! and initiate graceful shutdown.
//!
//! This is the **only** way to manage tasks dynamically at runtime.
//! [`Supervisor::run()`] is a self-contained entry point for static task sets.
//!
//! ## Why a separate handle?
//!
//! Separating the runtime management API into a handle prevents a common
//! misuse pattern: spawning `run(vec![])` in the background and then calling
//! `add()` — which silently fails because `run()` exits immediately
//! when the registry is empty.
//!
//! With this design, `add` is only available on `SupervisorHandle`,
//! which guarantees the runtime is alive and ready.

use std::sync::Arc;
use std::time::Duration;

use crate::error::RuntimeError;
use crate::tasks::TaskSpec;

use super::supervisor::Supervisor;

/// Handle for managing a running supervisor.
///
/// Obtained via [`Supervisor::serve()`]. Provides the full runtime
/// management API: add/remove/cancel tasks, query state, shutdown.
///
/// The handle holds an `Arc<Supervisor>` — cloning is cheap.
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
    /// The task will be spawned asynchronously by the registry listener.
    pub fn add(&self, spec: TaskSpec) -> Result<(), RuntimeError> {
        self.inner.add_task(spec)
    }

    /// Removes a task by name.
    ///
    /// The task will be cancelled and removed asynchronously.
    pub fn remove(&self, name: &str) -> Result<(), RuntimeError> {
        self.inner.remove_task(name)
    }

    /// Returns a sorted list of currently active task names.
    pub async fn list(&self) -> Vec<Arc<str>> {
        self.inner.list_tasks().await
    }

    /// Returns a sorted list of currently alive task names.
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
