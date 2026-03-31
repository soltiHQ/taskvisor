//! Core [`Task`] trait, [`TaskRef`] handle, and [`BoxTaskFuture`] alias.

use std::{future::Future, pin::Pin, sync::Arc};
use tokio_util::sync::CancellationToken;

use crate::error::TaskError;

/// Boxed, pinned, `Send` future: the return type of [`Task::spawn`].
pub type BoxTaskFuture = Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'static>>;

/// Shared handle to a task: `Arc<dyn Task>`.
pub type TaskRef = Arc<dyn Task>;

/// Async, cancelable unit of work managed by a [`Supervisor`](crate::Supervisor).
///
/// ## Contract
///
/// [`spawn`](Task::spawn) is called once per attempt and must return a fresh, independent future.
/// Implementations must observe `ctx.cancelled()` and return `Err(`[`TaskError::Canceled`]`)` promptly.
/// Non-cooperative tasks will be force-terminated after the grace period ([`GraceExceeded`](crate::RuntimeError::GraceExceeded)).
///
/// | Return value               | Meaning                 | Restarted?                                         |
/// |----------------------------|-------------------------|----------------------------------------------------|
/// | `Ok(())`                   | Task completed normally | Depends on [`RestartPolicy`](crate::RestartPolicy) |
/// | `Err(TaskError::Canceled)` | Cooperative shutdown    | Never                                              |
/// | `Err(TaskError::Fail)`     | Transient failure       | Per policy, with backoff                           |
/// | `Err(TaskError::Timeout)`  | Attempt timed out       | Per policy, with backoff                           |
/// | `Err(TaskError::Fatal)`    | Permanent failure       | Never                                              |
///
/// ## Cancellation
///
/// The [`CancellationToken`] `ctx` is cancelled by the supervisor during shutdown or when the task is removed at runtime.
/// - Long-running tasks should poll `ctx.cancelled()`: tasks that ignore the token will
///   block graceful shutdown until the grace period expires.
/// - Short-lived, one-shot tasks that complete quickly may omit this check.
///
/// A task that loops forever and only exits via `ctx.cancelled()` should return `Err(TaskError::Canceled)`;
/// A one-shot task that finishes its work should return `Ok(())`.
///
/// # Also
///
/// - For the closure-based implementation see [`TaskFn`](crate::TaskFn).
/// - To configure restart, backoff, and timeout see [`TaskSpec`](crate::TaskSpec).
pub trait Task: Send + Sync + 'static {
    /// Task name used in logs, metrics, and shutdown diagnostics.
    fn name(&self) -> &str;

    /// Creates a new future that runs the task until completion or cancellation.
    ///
    /// Called once per attempt. Takes `&self` - each call must return an
    /// independent future with no side effects from previous runs.
    fn spawn(&self, ctx: CancellationToken) -> BoxTaskFuture;
}
