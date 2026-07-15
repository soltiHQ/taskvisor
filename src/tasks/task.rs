//! The [`Task`] contract and shared task types.

use std::{future::Future, pin::Pin, sync::Arc};

use crate::error::TaskError;
use crate::tasks::TaskContext;

/// Boxed `Send` future returned by [`Task::spawn`].
pub type BoxTaskFuture = Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'static>>;

/// Shared task handle (`Arc<dyn Task>`).
pub type TaskRef = Arc<dyn Task>;

/// Async work managed by a [`Supervisor`](crate::Supervisor).
/// > Use [`TaskFn`](crate::TaskFn) unless you need a custom task type.
///
/// ## Attempt Contract
///
/// The supervisor calls [`spawn`](Task::spawn) once for every attempt.
/// > `spawn` **must return a new future on every call**.
///
/// The same task object is reused for every attempt.
/// State stored in its fields can therefore survive retries.
/// Each returned future is attempt-local; its local values do not carry over to the next attempt.
///
/// ```text
/// spawn(ctx) -> attempt 1 -> retry delay -> spawn(ctx) -> attempt 2
/// ```
///
/// ## Cancellation
///
/// Long-running tasks should listen to [`TaskContext::cancelled`] or use [`TaskContext::run_until_cancelled`].
/// Return [`TaskError::Canceled`] when the task stops for that reason.
/// > **This is a normal stop and is never retried.**
///
/// Short tasks may ignore cancellation if they finish quickly.
/// During shutdown, a task that does not stop before the grace period is aborted.
/// Dropping its future does not rollback external side effects.
///
/// | Result                  | Meaning           | Can restart?                                                      |
/// |-------------------------|-------------------|-------------------------------------------------------------------|
/// | `Ok(())`                | Attempt succeeded | Only with [`RestartPolicy::Always`](crate::RestartPolicy::Always) |
/// | [`TaskError::Fail`]     | Retryable failure | If policy and retry limit allow it                                |
/// | [`TaskError::Timeout`]  | Attempt timed out | If policy and retry limit allow it                                |
/// | [`TaskError::Canceled`] | Cooperative stop  | No                                                                |
/// | [`TaskError::Fatal`]    | Permanent failure | No                                                                |
///
/// A panic in `spawn` or in the returned future is caught and treated as a retryable failure.
/// > **Do not use panic for normal error handling.**
///
/// ## See Also
///
/// - For the closure-based implementation and a shared-state example, see [`TaskFn`](crate::TaskFn).
/// - To configure restart, backoff, and timeout see [`TaskSpec`](crate::TaskSpec).
pub trait Task: Send + Sync + 'static {
    /// Returns the stable name used for registration and observability.
    ///
    /// Names must be unique among registered tasks.
    /// A name can be used again after the previous task has finished removal and released it.
    fn name(&self) -> &str;

    /// Creates a new future for one attempt.
    ///
    /// Each call must return a fresh future.
    /// Use `ctx` to stop cooperatively.
    fn spawn(&self, ctx: TaskContext) -> BoxTaskFuture;
}
