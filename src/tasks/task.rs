//! # Task abstraction and function-backed task implementation.
//!
//! This module defines the [`Task`] trait (async, cancelable) and a convenient
//! function-backed implementation [`TaskFn`] (see `task_fn.rs`).
//! The common handle type is [`TaskRef`], an `Arc<dyn Task>` suitable for
//! sharing across the runtime.
//!
//! A task receives a [`CancellationToken`] and should periodically check it to
//! stop cooperatively during shutdown.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::error::TaskError;

/// Boxed future returned by a task spawn.
pub type BoxTaskFuture = Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'static>>;

/// # Asynchronous, cancelable unit.
///
/// A `Task` has a stable [`name`](Task::name) and a [`spawn`](Task::spawn) method that
/// **creates a fresh future** bound to a [`CancellationToken`].
///
/// Implementors should regularly check cancellation and exit promptly during shutdown.
///
/// ## Example
/// ```rust
/// use tokio_util::sync::CancellationToken;
/// use taskvisor::{Task, TaskError};
///
/// struct Demo;
///
/// impl Task for Demo {
///     fn name(&self) -> &str { "demo" }
///
///     fn spawn(&self, ctx: CancellationToken) -> taskvisor::BoxTaskFuture {
///         Box::pin(async move {
///             if ctx.is_cancelled() {
///                 return Ok(());
///             }
///             // do work...
///             Ok::<(), TaskError>(())
///         })
///     }
/// }
/// ```
pub trait Task: Send + Sync + 'static {
    /// Returns a stable, human-readable task name.
    fn name(&self) -> &str;

    /// Creates a **new** future that runs the task until completion or cancellation.
    ///
    /// Implementations should check `ctx.is_cancelled()` and exit quickly to
    /// honor graceful shutdown.
    fn spawn(&self, ctx: CancellationToken) -> BoxTaskFuture;
}

/// Shared handle to a task object used across the runtime.
pub type TaskRef = Arc<dyn Task>;
