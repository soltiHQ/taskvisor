//! Adapts an async closure to the [`Task`] contract.
//!
//! [`TaskFn`] is the short path from application code to a [`TaskRef`](crate::TaskRef).
//! The resulting task is placed in a [`TaskSpec`](crate::TaskSpec) before direct or controller admission.
//!
//! ```text
//! async closure ──► TaskFn ──► TaskRef ──► TaskSpec ──► admission
//! ```

use std::{future::Future, sync::Arc};

use crate::{
    error::TaskError,
    tasks::TaskContext,
    tasks::task::{BoxTaskFuture, Task},
};

/// A reusable [`Task`] backed by an async closure.
///
/// [`Task::spawn`] invokes the closure once for each attempt. The closure must create a fresh future on every call.
/// Captured state lives in the reusable closure; clone owned state into each returned future when needed.
///
/// One registration runs attempts sequentially. Reusing the same task under several [`TaskSpec`](crate::TaskSpec)
/// names can invoke the closure from separate actors.
///
/// # Long-running worker
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
/// # Task with shared state
///
/// Clone shared state into the closure, then into each attempt future:
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
/// # See also
///
/// - [`Task`] defines the attempt and cancellation contract.
/// - [`TaskSpec`](crate::TaskSpec) adds registration and execution settings.
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
    /// Wraps `f` as a concrete [`TaskFn`].
    ///
    /// Use this form when code needs the concrete adapter type.
    /// Use [`arc`](Self::arc) when the task will go directly into a [`TaskSpec`](crate::TaskSpec).
    pub fn new(f: F) -> Self {
        Self { f }
    }

    /// Wraps `f` in a task shared through [`Arc`].
    ///
    /// This is the shortest path from an async closure to a [`TaskSpec`](crate::TaskSpec).
    /// The returned `Arc<Self>` can coerce to [`TaskRef`](crate::TaskRef).
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
