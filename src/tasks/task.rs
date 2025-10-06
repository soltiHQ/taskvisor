//! # Task abstraction for supervised execution.
//!
//! Defines the core [`Task`] trait for async, cancelable units of work.
//!
//! - **[`Task`]** — trait for implementing async tasks with cancellation support
//! - **[`TaskRef`]** — shared handle (`Arc<dyn Task>`) for passing tasks across the runtime
//! - **[`BoxTaskFuture`]** — type alias for boxed task futures
//!
//! ## Rules
//! - The crate provides [`TaskFn`](crate::TaskFn) — a function-backed implementation that wraps closures as tasks.
//! - Tasks receive a [`CancellationToken`] and **must** observe cancellation at safe points.
//!
//! ## Return semantics
//! - `Ok(())` — task completed successfully (restart policy applies).
//! - `Err(TaskError::Canceled)` — cooperative shutdown (not considered a failure).
//! - `Err(TaskError::Fail | TaskError::Timeout)` — retryable failures.
//! - `Err(TaskError::Fatal)` — non-retryable, actor terminates as dead.

use std::{future::Future, pin::Pin, sync::Arc};

use tokio_util::sync::CancellationToken;

use crate::error::TaskError;

/// Boxed future returned by [`Task::spawn`].
///
/// This is a type alias for `Pin<Box<dyn Future<...>>>`:
/// - **Boxed**: required for trait objects (dynamic dispatch)
/// - **Pinned**: required for async futures (self-referential structs)
/// - **Send**: task futures can be sent across threads
pub type BoxTaskFuture = Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'static>>;

/// Shared handle to a task object.
///
/// Type alias for `Arc<dyn Task>`, used throughout the runtime for:
/// - Passing tasks to [`Supervisor`](crate::Supervisor)
/// - Sharing tasks between actors
/// - Cloning task references cheaply
pub type TaskRef = Arc<dyn Task>;

/// Asynchronous, cancelable unit of work.
///
/// A `Task` represents a unit of work that can be:
/// - **Spawned multiple times** (via [`spawn`](Task::spawn))
/// - **Cancelled cooperatively** (via [`CancellationToken`])
/// - **Supervised** (by [`Supervisor`](crate::Supervisor))
///
/// ## Requirements
/// - **Cancellation**: implementations **must** observe `ctx.cancelled()` at safe await points and
///   return `Err(TaskError::Canceled)` promptly on shutdown.
///
/// ## Example
/// ```rust
/// use std::{future::Future, pin::Pin, time::Duration};
/// use tokio_util::sync::CancellationToken;
/// use taskvisor::{Task, TaskError};
///
/// struct MyTask;
///
/// impl Task for MyTask {
///     fn name(&self) -> &str { "my-task" }
///
///     fn spawn(&self, ctx: CancellationToken)
///         -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'static>>
///     {
///         Box::pin(async move {
///             loop {
///                 // Do one unit of work (replace with real IO/compute)
///                 // Safe point with cancellation via select
///                 tokio::select! {
///                     _ = ctx.cancelled() => {
///                         return Err(TaskError::Canceled);
///                     }
///                     _ = tokio::time::sleep(Duration::from_millis(200)) => {
///                         // work chunk finished; continue loop
///                     }
///                 }
///             }
///         })
///     }
/// }
/// ```
pub trait Task: Send + Sync + 'static {
    /// Returns a stable, human-readable task name.
    ///
    /// Used for logging, metrics, and stuck task detection during shutdown.
    fn name(&self) -> &str;

    /// Creates a new Future that runs the task until completion or cancellation.
    ///
    /// ### Cancellation requirements
    /// Implementations must observe `ctx.cancelled()` at safe `await` points and return
    /// `Err(TaskError::Canceled)` promptly upon shutdown.
    ///
    /// ### Stateless execution
    /// This method takes `&self` (not `&mut self`), meaning:
    /// - Safe to call from multiple actors concurrently
    /// - Each call returns an independent future
    /// - No shared mutable state between spawns
    fn spawn(&self, ctx: CancellationToken) -> BoxTaskFuture;
}
