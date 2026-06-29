//! Closure-based [`Task`] implementation.

use std::{future::Future, sync::Arc};

use crate::{
    error::TaskError,
    tasks::TaskContext,
    tasks::task::{BoxTaskFuture, Task},
};

/// Closure-based [`Task`] implementation.
///
/// `TaskFn` is the easiest way to create a task.
/// Pass a name and a closure that returns an async block.
///
/// The closure is called on every attempt, so it creates a new future each time.
///
/// ## Simple Task
///
/// ```rust
/// use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
///
/// let worker: TaskRef = TaskFn::arc("worker", |_ctx: TaskContext| async move {
///     Ok::<(), TaskError>(())
/// });
/// ```
///
/// ## Task With Shared State
///
/// ```rust
/// use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
/// use std::time::Duration;
///
/// use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
///
/// let counter = Arc::new(AtomicU64::new(0));
/// let task: TaskRef = TaskFn::arc("counter", {
///     let counter = counter.clone();
///     move |ctx: TaskContext| {
///         let counter = counter.clone();
///         async move {
///             loop {
///                 tokio::select! {
///                     _ = ctx.cancelled() => return Err(TaskError::Canceled),
///                     _ = tokio::time::sleep(Duration::from_secs(1)) => {
///                         counter.fetch_add(1, Ordering::Relaxed);
///                     }
///                 }
///             }
///         }
///     }
/// });
/// ```
///
/// ## Also
///
/// - See the [`Task`](crate::Task) trait documentation.
/// - To configure restart, backoff, and timeout see [`TaskSpec`](crate::TaskSpec).
#[derive(Debug)]
pub struct TaskFn<F> {
    name: Arc<str>,
    f: F,
}

impl<F> TaskFn<F> {
    /// Creates a task from a name and closure.
    pub fn new(name: impl Into<Arc<str>>, f: F) -> Self {
        Self {
            name: name.into(),
            f,
        }
    }

    /// Creates a shared task handle.
    ///
    /// The return value can be assigned to [`TaskRef`](crate::TaskRef):
    /// `let task: TaskRef = TaskFn::arc("name", f);`.
    pub fn arc(name: impl Into<Arc<str>>, f: F) -> Arc<Self> {
        Arc::new(Self::new(name, f))
    }
}

impl<Fnc, Fut> Task for TaskFn<Fnc>
where
    Fnc: Fn(TaskContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), TaskError>> + Send + 'static,
{
    fn name(&self) -> &str {
        &self.name
    }

    fn spawn(&self, ctx: TaskContext) -> BoxTaskFuture {
        let fut = (self.f)(ctx);
        Box::pin(fut)
    }
}