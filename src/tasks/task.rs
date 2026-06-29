//! Core [`Task`] trait, [`TaskRef`] handle, and [`BoxTaskFuture`] alias.

use std::{future::Future, pin::Pin, sync::Arc};

use crate::error::TaskError;
use crate::tasks::TaskContext;

/// Boxed, pinned, `Send` future: the return type of [`Task::spawn`].
pub type BoxTaskFuture = Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'static>>;

/// Shared task handle: `Arc<dyn Task>`.
pub type TaskRef = Arc<dyn Task>;

/// Async unit of work managed by a [`Supervisor`](crate::Supervisor).
///
/// ## Contract
///
/// [`spawn`](Task::spawn) is called for each attempt.
/// It _must_ create a new future every time.
///
/// The same task may be started many times when a restart policy is used.
/// Shared state is allowed, but the returned future must not be reused.
///
/// ## Cancellation
///
/// Long-running tasks must listen for cancellation.
/// Use [`TaskContext::cancelled`] or [`TaskContext::is_cancelled`].
///
/// When a task stops because of cancellation, it should return `Err(TaskError::Canceled)`.
/// This lets the runtime report it as a graceful cancellation.
///
/// Short or one-shot tasks may ignore cancellation if they finish quickly.
/// Tasks that ignore cancellation during shutdown may be force-aborted after the supervisor grace period.
///
/// | Return value               | Meaning              | Restarted?                                         |
/// |----------------------------|----------------------|----------------------------------------------------|
/// | `Ok(())`                   | Completed            | Depends on [`RestartPolicy`](crate::RestartPolicy) |
/// | `Err(TaskError::Fail)`     | Retryable failure    | Per policy, with backoff                           |
/// | `Err(TaskError::Timeout)`  | Attempt timed out    | Per policy, with backoff                           |
/// | `Err(TaskError::Canceled)` | Graceful cancel      | Never                                              |
/// | `Err(TaskError::Fatal)`    | Permanent failure    | Never                                              |
///
/// # Also
///
/// - For the closure-based implementation see [`TaskFn`](crate::TaskFn).
/// - To configure restart, backoff, and timeout see [`TaskSpec`](crate::TaskSpec).
pub trait Task: Send + Sync + 'static {
    /// Task name used in logs, metrics, and shutdown diagnostics.
    ///
    /// Names must be unique among **currently registered** tasks, but may be reused after the previous holder is removed.
    fn name(&self) -> &str;

    /// Creates a new future for one task attempt.
    ///
    /// Called once per attempt. Each call must return a fresh future.
    fn spawn(&self, ctx: TaskContext) -> BoxTaskFuture;
}
