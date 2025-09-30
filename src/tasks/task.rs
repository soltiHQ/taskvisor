//! # Task abstraction and function-backed task implementation.
//!
//! This module defines the [`Task`] trait (async, cancelable) and a convenient function-backed implementation [`TaskFn`].
//! The common handle type is [`TaskRef`], an `Arc<dyn Task>` suitable for sharing across the runtime.
//!
//! A task receives a [`CancellationToken`] and should periodically check it to
//! stop cooperatively during shutdown.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::error::TaskError;

/// # Asynchronous, cancelable unit.
///
/// A `Task` has a stable [`name`](Task::name) and an async [`run`](Task::run) method that receives a [`CancellationToken`].
/// Implementors should regularly check cancellation and exit promptly during shutdown.
///
/// # Example
/// ```
/// use tokio_util::sync::CancellationToken;
/// use async_trait::async_trait;
/// use taskvisor::{Task, TaskError};
///
/// struct Demo;
///
/// #[async_trait]
/// impl Task for Demo {
///     fn name(&self) -> &str { "demo" }
///
///     async fn run(&self, ctx: CancellationToken) -> Result<(), TaskError> {
///         if ctx.is_cancelled() {
///             return Ok(());
///         }
///         // do work...
///         Ok(())
///     }
/// }
/// ```
#[async_trait]
pub trait Task: Send + Sync + 'static {
    /// Returns a stable, human-readable task name.
    fn name(&self) -> &str;

    /// Executes the task until completion or cancellation.
    ///
    /// Implementations should check `ctx.is_cancelled()` and exit quickly to honor graceful shutdown.
    async fn run(&self, ctx: CancellationToken) -> Result<(), TaskError>;
}
