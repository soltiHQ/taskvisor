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
pub struct TaskFn<F> {
    name: Arc<str>,
    f: F,
}

impl<F> std::fmt::Debug for TaskFn<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskFn")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
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
    /// The return value can be assigned to [`TaskRef`](crate::TaskRef): `let task: TaskRef = TaskFn::arc("name", f);`.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_util::sync::CancellationToken;

    fn ctx() -> TaskContext {
        TaskContext::from_token(CancellationToken::new())
    }

    #[test]
    fn name_round_trips() {
        let t = TaskFn::new("worker-7", |_ctx: TaskContext| async { Ok(()) });
        assert_eq!(t.name(), "worker-7");
    }

    #[test]
    fn spawn_invokes_closure_once_per_call() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let t = TaskFn::new("counter", move |_ctx: TaskContext| {
            counter.fetch_add(1, Ordering::SeqCst);
            async { Ok(()) }
        });

        for _ in 0..3 {
            drop(t.spawn(ctx()));
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "spawn must invoke the closure (a fresh future) on every attempt"
        );
    }

    #[test]
    fn debug_works_for_closure_backed_task_and_shows_name() {
        let t = TaskFn::new("dbg", |_ctx: TaskContext| async { Ok::<(), TaskError>(()) });
        let rendered = format!("{t:?}");
        assert!(
            rendered.contains("TaskFn"),
            "debug must name the type: {rendered}"
        );
        assert!(
            rendered.contains("dbg"),
            "debug must include the task name: {rendered}"
        );
    }
}
