//! Defines the executable side of a supervised task.
//!
//! [`Task`] is a factory for attempt futures. Application code usually creates one with [`TaskFn`](crate::TaskFn),
//! or implements the trait for a named type. [`TaskRef`] erases that concrete type for [`TaskSpec`](crate::TaskSpec).
//! After admission, Taskvisor calls [`Task::spawn`] once for each attempt and polls the returned [`BoxTaskFuture`].
//!
//! ```text
//! application task ──► TaskRef ──► TaskSpec ──► registry admission
//!                                                    ▼
//!                                                TaskActor
//!                                                    ▼
//!                                                run_once
//!                                                    ▼
//!                         Task::spawn(TaskContext) ──► BoxTaskFuture
//! ```

use std::{future::Future, pin::Pin, sync::Arc};

use crate::error::TaskError;
use crate::tasks::TaskContext;

/// Type-erased future for one task attempt.
///
/// [`Task::spawn`] returns this future for the attempt runner to poll.
pub type BoxTaskFuture = Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'static>>;

/// Shared, type-erased [`Task`] handle used by [`TaskSpec`](crate::TaskSpec).
pub type TaskRef = Arc<dyn Task>;

/// A factory for supervised attempt futures.
///
/// Task identity belongs to [`TaskSpec`](crate::TaskSpec), not to the executable object.
/// The same [`TaskRef`] can be registered through different specs.
/// Separate registrations may call [`spawn`](Task::spawn) concurrently on that shared object.
///
/// # Attempt contract
///
/// The actor reuses the task object. Its attempt runner calls [`spawn`](Task::spawn) once per attempt.
/// Each call must return a new future. Fields in the task object may keep state across retries;
/// values owned by the returned future belong only to that attempt.
///
/// ```text
/// Task object
///      ├── spawn(ctx) ────────► attempt 1
///      └── later spawn(ctx) ──► attempt 2
/// ```
///
/// # Implementing a named task
///
/// ```rust
/// use taskvisor::{BoxTaskFuture, Task, TaskContext, TaskError};
///
/// struct Worker;
///
/// impl Task for Worker {
///     fn spawn(&self, ctx: TaskContext) -> BoxTaskFuture {
///         Box::pin(async move {
///             ctx.cancelled().await;
///             Err(TaskError::Canceled)
///         })
///     }
/// }
/// ```
///
/// # Cancellation
///
/// Long-running tasks must observe [`TaskContext`] and return [`TaskError::Canceled`] after a cooperative stop.
/// Cancellation is never retried. A task that does not stop within the removal or shutdown grace window may be aborted.
/// Timeout drops the future inside the attempt runner. Abort asks Tokio to drop it after the current poll returns.
/// Neither action rolls back external side effects or interrupts synchronous code inside a poll.
///
/// Taskvisor drops every attempt future synchronously on its Tokio worker. Keep destructors for future-owned values
/// short and non-blocking. A blocking destructor delays attempt release and holds any concurrency permit until it returns.
///
/// # Attempt results
///
/// | Result                  | Actor decision                                      |
/// |-------------------------|-----------------------------------------------------|
/// | `Ok(())`                | Repeat only under `RestartPolicy::Always`           |
/// | [`TaskError::Fail`]     | Retry when policy and retry limit allow it          |
/// | [`TaskError::Timeout`]  | Retry when policy and retry limit allow it          |
/// | [`TaskError::Canceled`] | Stop                                                |
/// | [`TaskError::Fatal`]    | Stop                                                |
///
/// A primary panic while creating or polling the future is classified as [`TaskError::Fail`].
/// A cleanup panic while dropping user-owned attempt data stops normal retry handling.
/// Do not use panic for expected failures.
///
/// # See also
///
/// - [`TaskFn`](crate::TaskFn) adapts an async closure.
/// - [`TaskSpec`](crate::TaskSpec) adds identity and execution settings.
pub trait Task: Send + Sync + 'static {
    /// Creates a fresh future for one attempt.
    ///
    /// This method runs synchronously before the attempt timeout starts.
    /// Return the future quickly and perform task work inside it.
    /// Use `ctx` inside the future for cooperative cancellation.
    fn spawn(&self, ctx: TaskContext) -> BoxTaskFuture;
}
