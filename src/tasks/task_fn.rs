//! # Function-backed task (`TaskFn`)
//!
//! [`TaskFn`] wraps a closure `F: Fn(CancellationToken) -> Fut`, producing a fresh
//! future per spawn. This avoids shared mutable state and не требует `Mutex`.
//!
//! ## Concurrency semantics
//! - Каждый вызов [`TaskFn::spawn`] создаёт **новый** future, владеющий своим state.
//! - Без скрытой мутации между рестартами; если нужен общий state — используйте `Arc<...>`
//!   явно внутри замыкания.
//!
//! ## Example
//! ```rust
//! use tokio_util::sync::CancellationToken;
//! use taskvisor::{TaskFn, TaskRef, TaskError};
//!
//! let t: TaskRef = TaskFn::arc("worker", |ctx: CancellationToken| async move {
//!     if ctx.is_cancelled() {
//!         return Ok(());
//!     }
//!     // do work...
//!     Ok::<_, TaskError>(())
//! });
//!
//! assert_eq!(t.name(), "worker");
//! ```

use std::borrow::Cow;
use std::future::Future;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::error::TaskError;
use crate::tasks::task::{BoxTaskFuture, Task};

/// Function-backed task implementation.
///
/// Wraps a closure that *creates* a new future per spawn.
#[derive(Debug)]
pub struct TaskFn<F> {
    name: Cow<'static, str>,
    f: F,
}

impl<F> TaskFn<F> {
    /// Creates a new function-backed task.
    ///
    /// Prefer [`TaskFn::arc`] when you immediately need a [`TaskRef`].
    pub fn new(name: impl Into<Cow<'static, str>>, f: F) -> Self {
        Self { name: name.into(), f }
    }

    /// Creates the task and returns it as a shared handle (`Arc<dyn Task>`).
    ///
    /// ## Example
    /// ```rust
    /// use tokio_util::sync::CancellationToken;
    /// use taskvisor::{TaskFn, TaskRef, TaskError};
    ///
    /// let t: TaskRef = TaskFn::arc("hello", |_ctx: CancellationToken| async {
    ///     Ok::<_, TaskError>(())
    /// });
    /// assert_eq!(t.name(), "hello");
    /// ```
    pub fn arc(name: impl Into<Cow<'static, str>>, f: F) -> Arc<Self> {
        Arc::new(Self::new(name, f))
    }
}

impl<F, Fut> Task for TaskFn<F>
where
    F: Fn(CancellationToken) -> Fut + Send + Sync + 'static, // Fn, not FnMut
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