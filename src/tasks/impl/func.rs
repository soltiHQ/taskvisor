//! Closure-based [`Task`] implementation.

use std::{future::Future, sync::Arc};
use tokio_util::sync::CancellationToken;

use crate::{
    error::TaskError,
    tasks::task::{BoxTaskFuture, Task},
};

/// Closure-based [`Task`] implementation.
///
/// - Wraps `F: Fn(CancellationToken) -> Future`
/// - Each [`spawn`](Task::spawn) invokes the closure to produce a fresh, independent future.
///
/// ## Stateless task
///
/// ```rust
/// use tokio_util::sync::CancellationToken;
/// use taskvisor::{TaskFn, TaskRef, TaskError};
///
/// let worker: TaskRef = TaskFn::arc("worker", |_ctx: CancellationToken| async move {
///     // do some work and complete
///     Ok(())
/// });
/// ```
///
/// ## Stateful task (shared state via `Arc`)
///
/// ```rust
/// use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
/// use std::time::Duration;
/// use tokio_util::sync::CancellationToken;
///
/// use taskvisor::{TaskFn, TaskRef, TaskError};
///
/// let counter = Arc::new(AtomicU64::new(0));
/// let task: TaskRef = TaskFn::arc("counter", {
///     let counter = counter.clone();
///     move |ctx: CancellationToken| {
///         // clone per-attempt; the underlying value persists across restarts.
///         let counter = counter.clone();
///         async move {
///             loop {
///                 tokio::select! {
///                     _ = ctx.cancelled() => return Err(TaskError::Canceled),
///                     _ = tokio::time::sleep(Duration::from_secs(1)) => {
///                         let _ = counter.fetch_add(1, Ordering::Relaxed);
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
    /// Creates a new task.
    pub fn new(name: impl Into<Arc<str>>, f: F) -> Self {
        Self {
            name: name.into(),
            f,
        }
    }

    /// Creates the task as a [`TaskRef`](crate::TaskRef) ready to pass to the supervisor.
    pub fn arc(name: impl Into<Arc<str>>, f: F) -> Arc<Self> {
        Arc::new(Self::new(name, f))
    }
}

impl<Fnc, Fut> Task for TaskFn<Fnc>
where
    Fnc: Fn(CancellationToken) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), TaskError>> + Send + 'static,
{
    fn name(&self) -> &str {
        &self.name
    }

    fn spawn(&self, ctx: CancellationToken) -> BoxTaskFuture {
        let fut = (self.f)(ctx);
        Box::pin(fut)
    }
}
