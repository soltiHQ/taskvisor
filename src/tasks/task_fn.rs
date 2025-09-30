use crate::{Task, TaskError};
use async_trait::async_trait;
use std::borrow::Cow;
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// # Shared handle to a task object.
///
/// This is the primary type used by the supervisor and specs.
pub type TaskRef = std::sync::Arc<dyn Task>;

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
/// use std::future::Future;
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
