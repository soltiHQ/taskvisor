//! Create a [`Task`] from an async closure.

use std::{future::Future, sync::Arc};

use crate::{
    error::TaskError,
    tasks::TaskContext,
    tasks::task::{BoxTaskFuture, Task},
};

/// A task backed by an async closure.
///
/// This is the simplest way to define a task.
/// The supervisor calls the closure for every attempt. Each call creates a fresh future.
/// [`TaskSpec`](crate::TaskSpec) owns the task's registration name.
///
/// ## Long-Running Worker
///
/// No type annotations are needed: the constructor bounds drive inference.
///
/// ```rust
/// use taskvisor::{TaskError, TaskFn, TaskRef};
///
/// let worker: TaskRef = TaskFn::arc(|ctx| async move {
///     loop {
///         tokio::select! {
///             _ = ctx.cancelled() => return Err(TaskError::Canceled),
///             _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
///                 // Do one unit of work.
///             }
///         }
///     }
/// });
/// ```
///
/// ## Task With Shared State
///
/// The closure can run more than once.
/// Clone shared state into the closure, then clone it again into each returned future:
///
/// ```rust
/// use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
/// use std::time::Duration;
///
/// use taskvisor::TaskFn;
///
/// let counter = Arc::new(AtomicU64::new(0));
/// let task = TaskFn::arc({
///     let counter = counter.clone();
///     move |ctx| {
///         let counter = counter.clone();
///         async move {
///             loop {
///                 ctx.run_until_cancelled(tokio::time::sleep(Duration::from_secs(1)))
///                     .await?;
///                 counter.fetch_add(1, Ordering::Relaxed);
///             }
///         }
///     }
/// });
/// ```
///
/// ## See Also
///
/// - See the [`Task`] trait documentation.
/// - To configure restart, backoff, and timeout see [`TaskSpec`](crate::TaskSpec).
pub struct TaskFn<F> {
    f: F,
}

impl<F> std::fmt::Debug for TaskFn<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskFn").finish_non_exhaustive()
    }
}

impl<F, Fut> TaskFn<F>
where
    F: Fn(TaskContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), TaskError>> + Send + 'static,
{
    /// Creates a task from a closure.
    ///
    /// Rust normally infers the context and error types from these bounds.
    pub fn new(f: F) -> Self {
        Self { f }
    }

    /// Creates a task inside an [`Arc`].
    ///
    /// The result converts to [`TaskRef`](crate::TaskRef) where needed.
    pub fn arc(f: F) -> Arc<Self> {
        Arc::new(Self::new(f))
    }
}

impl<Fnc, Fut> Task for TaskFn<Fnc>
where
    Fnc: Fn(TaskContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), TaskError>> + Send + 'static,
{
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
    fn constructors_infer_closure_types() {
        let inferred = TaskFn::arc(|ctx| async move {
            if ctx.is_cancelled() {
                return Err(TaskError::Canceled);
            }
            Ok(())
        });
        let direct = TaskFn::new(|_ctx: TaskContext| async { Ok(()) });

        drop(inferred.spawn(ctx()));
        drop(direct.spawn(ctx()));
    }

    #[test]
    fn spawn_invokes_closure_once_per_call() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let t = TaskFn::new(move |_ctx: TaskContext| {
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
    fn debug_works_for_closure_backed_task() {
        let t = TaskFn::new(|_ctx: TaskContext| async { Ok::<(), TaskError>(()) });
        let rendered = format!("{t:?}");
        assert!(
            rendered.contains("TaskFn"),
            "debug must name the type: {rendered}"
        );
    }
}
