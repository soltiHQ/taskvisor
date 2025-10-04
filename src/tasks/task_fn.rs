//! # Function-backed task implementation.
//!
//! Provides [`TaskFn`] — a task implementation that wraps closures.
//!
//! ## Architecture
//! ```text
//! TaskFn<F> where F: Fn(CancellationToken) -> Future
//!     │
//!     └─► Each spawn() call creates a NEW future
//!         └─► Future owns its state (no sharing between restarts)
//! ```
//!
//! ## Example
//! ```rust
//! use tokio_util::sync::CancellationToken;
//! use taskvisor::{TaskFn, TaskRef, TaskError};
//! use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
//!
//! // Stateless task (no shared state):
//! let simple: TaskRef = TaskFn::arc("simple", |ctx: CancellationToken| async move {
//!     while !ctx.is_cancelled() {
//!         println!("working...");
//!         tokio::time::sleep(std::time::Duration::from_secs(1)).await;
//!     }
//!     Ok(())
//! });
//!
//! // Stateful task (with Arc for shared state):
//! let counter = Arc::new(AtomicU64::new(0));
//! let stateful: TaskRef = TaskFn::arc("counter", {
//!     let counter = counter.clone();
//!     move |ctx: CancellationToken| {
//!         let counter = counter.clone();
//!         async move {
//!             while !ctx.is_cancelled() {
//!                 let n = counter.fetch_add(1, Ordering::Relaxed);
//!                 println!("count: {}", n);
//!                 tokio::time::sleep(std::time::Duration::from_secs(1)).await;
//!             }
//!             Ok(())
//!         }
//!     }
//! });
//! ```

use std::{borrow::Cow, future::Future, sync::Arc};

use tokio_util::sync::CancellationToken;

use crate::{
    error::TaskError,
    tasks::task::{BoxTaskFuture, Task},
};

/// Function-backed task implementation.
///
/// Wraps a closure `F: Fn(CancellationToken) -> Future` that creates a fresh future on each [`spawn`](Task::spawn) call.
///
/// ### Rules
/// - Each spawn creates an independent future (no shared state)
/// - For shared state, use `Arc<Mutex<T>>` explicitly in closure
/// - Closure is no mutable self
pub struct TaskFn<F> {
    name: Cow<'static, str>,
    f: F,
}

impl<F> TaskFn<F> {
    /// Creates a new function-backed task with the given name.
    ///
    /// ### Parameters
    /// - `name`: Task name (for logging, metrics)
    /// - `f`: Closure that creates a future when called
    ///
    /// ### Notes
    /// Prefer [`TaskFn::arc`] when you need a [`TaskRef`](crate::TaskRef) immediately.
    pub fn new(name: impl Into<Cow<'static, str>>, f: F) -> Self {
        Self {
            name: name.into(),
            f,
        }
    }

    /// Creates the task and returns it as a shared handle (`Arc<dyn Task>`).
    ///
    /// This is the most common way to create a TaskFn.
    ///
    /// ### Example
    /// ```rust
    /// use tokio_util::sync::CancellationToken;
    /// use taskvisor::{TaskFn, TaskRef, TaskError};
    ///
    /// let t: TaskRef = TaskFn::arc("hello", |_ctx: CancellationToken| async {
    ///     println!("Hello from task!");
    ///     Ok::<_, TaskError>(())
    /// });
    /// assert_eq!(t.name(), "hello");
    /// ```
    pub fn arc(name: impl Into<Cow<'static, str>>, f: F) -> Arc<Self> {
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
