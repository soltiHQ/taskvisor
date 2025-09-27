//! # Task abstraction and function-backed task implementation.
//!
//! This module defines the [`Task`] trait (async, cancelable) and a convenient function-backed implementation [`TaskFn`].
//! The common handle type is [`TaskRef`], an `Arc<dyn Task>` suitable for sharing across the runtime.
//!
//! A task receives a [`CancellationToken`] and should periodically check it to
//! stop cooperatively during shutdown.

use std::{borrow::Cow, future::Future, sync::Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::error::TaskError;

/// # Shared handle to a task object.
///
/// This is the primary type used by the supervisor and specs.
pub type TaskRef = std::sync::Arc<dyn Task>;

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

/// # Function-backed task implementation.
///
/// [`TaskFn`] wraps a closure `Fnc: FnMut(CancellationToken) -> Fut`.
/// The closure is protected by a [`Mutex`] to allow calling `run(&self, ...)` multiple times even though the closure is `FnMut`.
/// Use [`TaskFn::arc`] for a one-liner that returns a [`TaskRef`].
///
/// ### Concurrency semantics:
/// `TaskFn` uses a mutex to safely invoke the `FnMut` closure. The mutex is held ONLY during
/// the creation of the future (calling the closure), not during its execution.
///
/// This means:
/// - Multiple calls to `run()` can execute concurrently after their futures are created
/// - The mutex prevents data races when accessing the closure's captured state
/// - There's no performance bottleneck from long-running async operations
///
/// ### Note:
/// If your closure captures mutable state that's accessed INSIDE the returned future,
/// you must add your own synchronization (Arc<Mutex<_>>, etc.) as the TaskFn's mutex
/// doesn't protect the future's execution, only its creation.
///
/// # Example
/// ```
/// use tokio_util::sync::CancellationToken;
/// use taskvisor::{TaskFn, TaskRef, TaskError};
///
/// let t: TaskRef = TaskFn::arc("worker", |ctx: CancellationToken| async move {
///     if ctx.is_cancelled() {
///         return Ok(());
///     }
///     // do work...
///     Ok::<_, TaskError>(())
/// });
///
/// assert_eq!(t.name(), "worker");
/// ```
#[derive(Debug)]
pub struct TaskFn<Fnc, Fut>
where
    Fnc: FnMut(CancellationToken) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), TaskError>> + Send + 'static,
{
    /// Stable task name.
    name: Cow<'static, str>,
    /// Underlying function (guarded by a mutex to allow `FnMut` with `&self`).
    func: Mutex<Fnc>,
}

impl<Fnc, Fut> TaskFn<Fnc, Fut>
where
    Fnc: FnMut(CancellationToken) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), TaskError>> + Send + 'static,
{
    /// Creates a new function-backed task.
    ///
    /// Prefer [`TaskFn::arc`] when you immediately need a [`TaskRef`].
    pub fn new(name: impl Into<Cow<'static, str>>, func: Fnc) -> Self {
        Self {
            name: name.into(),
            func: Mutex::new(func),
        }
    }

    /// Creates the task and returns it as a shared handle (`Arc<dyn Task>`).
    ///
    /// # Example
    /// ```
    /// use tokio_util::sync::CancellationToken;
    /// use taskvisor::{TaskFn, TaskRef, TaskError};
    ///
    /// let t: TaskRef = TaskFn::arc("hello", |_ctx: CancellationToken| async { Ok::<_, TaskError>(()) });
    /// assert_eq!(t.name(), "hello");
    /// ```
    pub fn arc(name: impl Into<Cow<'static, str>>, func: Fnc) -> TaskRef {
        std::sync::Arc::new(Self::new(name, func))
    }
}

#[async_trait]
impl<Fnc, Fut> Task for TaskFn<Fnc, Fut>
where
    Fnc: FnMut(CancellationToken) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), TaskError>> + Send + 'static,
{
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self, ctx: CancellationToken) -> Result<(), TaskError> {
        let fut = {
            let mut f = self.func.lock().map_err(|_| TaskError::Fatal {
                error: "mutex poisoned".into(),
            })?;
            (f)(ctx)
        };
        fut.await
    }
}
