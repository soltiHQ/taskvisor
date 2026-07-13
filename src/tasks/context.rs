//! Task execution context handed to [`Task::spawn`](crate::Task::spawn).

use std::future::Future;

use tokio_util::sync::CancellationToken;

use crate::error::TaskError;

/// Context passed to a [`Task`](crate::Task) for one attempt.
///
/// It tells the task when it should stop.
///
/// ## Cancellation
///
/// The context is cancelled by the supervisor during shutdown or when the task is removed at runtime.
/// - Short-lived, one-shot tasks that finish quickly may ignore it.
/// - Long-running tasks should wrap their awaits in [`run_until_cancelled`](Self::run_until_cancelled),
///   or await [`cancelled`](Self::cancelled) / check [`is_cancelled`](Self::is_cancelled) manually.
#[derive(Clone, Debug)]
pub struct TaskContext {
    cancel: CancellationToken,
}

impl TaskContext {
    /// Wraps a raw cancellation token (crate-internal; the runtime owns token creation).
    pub(crate) fn from_token(cancel: CancellationToken) -> Self {
        Self { cancel }
    }

    /// A task context that is not connected to a supervisor.
    ///
    /// It starts active. Use it in tests when you want to call [`Task::spawn`](crate::Task::spawn)
    /// or any function that takes a [`TaskContext`] without starting a supervisor.
    ///
    /// ```rust
    /// use taskvisor::TaskContext;
    ///
    /// let ctx = TaskContext::detached();
    /// assert!(!ctx.is_cancelled());
    /// ```
    #[cfg(feature = "test-util")]
    #[cfg_attr(docsrs, doc(cfg(feature = "test-util")))]
    #[must_use]
    pub fn detached() -> Self {
        Self::from_token(CancellationToken::new())
    }

    /// A task context that is already cancelled.
    ///
    /// Use it in tests to check cancellation code:
    /// [`run_until_cancelled`](Self::run_until_cancelled) returns
    /// [`Err(TaskError::Canceled)`](TaskError::Canceled) without polling the future.
    ///
    /// ```rust
    /// use taskvisor::TaskContext;
    ///
    /// let ctx = TaskContext::detached_cancelled();
    /// assert!(ctx.is_cancelled());
    /// ```
    #[cfg(feature = "test-util")]
    #[cfg_attr(docsrs, doc(cfg(feature = "test-util")))]
    #[must_use]
    pub fn detached_cancelled() -> Self {
        let token = CancellationToken::new();
        token.cancel();
        Self::from_token(token)
    }

    /// Waits until the context is cancelled.
    ///
    /// Resolves immediately if the context is already cancelled.
    /// Safe to call repeatedly and to use as a branch in `tokio::select!`.
    pub async fn cancelled(&self) {
        self.cancel.cancelled().await;
    }

    /// Returns `true` if the context has already been cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Runs a future until it completes or this context is cancelled.
    ///
    /// Returns `Ok(output)` when `fut` completes first.
    /// Returns [`Err(TaskError::Canceled)`](TaskError::Canceled) when the context is cancelled first.
    ///
    /// Cancellation wins ties: if the context is already cancelled, `fut` is not polled at all.
    /// On cancellation `fut` is dropped together with any work it owns.
    ///
    /// Use it instead of the manual `tokio::select!` + `Err(TaskError::Canceled)` pattern:
    ///
    /// ```rust,no_run
    /// use std::time::Duration;
    /// use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
    ///
    /// # async fn poll_once() -> Result<(), TaskError> { Ok(()) }
    /// let poller: TaskRef = TaskFn::arc("poller", |ctx| async move {
    ///     loop {
    ///         // `?` propagates poll_once() errors.
    ///         // On shutdown this returns TaskError::Canceled (clean stop, not a failure).
    ///         ctx.run_until_cancelled(poll_once()).await??;
    ///         ctx.run_until_cancelled(tokio::time::sleep(Duration::from_secs(5)))
    ///             .await?;
    ///     }
    /// });
    /// ```
    pub async fn run_until_cancelled<F: Future>(&self, fut: F) -> Result<F::Output, TaskError> {
        tokio::select! {
            biased;
            _ = self.cancelled() => Err(TaskError::Canceled),
            output = fut => Ok(output),
        }
    }

    /// Creates a child context.
    ///
    /// Use it to give a piece of sub-work its own cancellation scope.
    ///
    /// The child is cancelled when this context is cancelled.
    /// Cancelling the child (through interop APIs) does not cancel this context.
    #[must_use]
    pub fn child(&self) -> TaskContext {
        TaskContext {
            cancel: self.cancel.child_token(),
        }
    }

    /// Returns the raw [`tokio_util`] cancellation token.
    ///
    /// Use this only when another API needs a `CancellationToken`.
    /// The returned token shares cancellation state with this context.
    ///
    /// Requires the `tokio-util-interop` feature.
    #[cfg(feature = "tokio-util-interop")]
    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancel.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::TaskContext;
    use crate::error::TaskError;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    #[cfg(feature = "test-util")]
    #[tokio::test]
    async fn detached_context_starts_active() {
        let ctx = TaskContext::detached();

        assert!(!ctx.is_cancelled());
        let out = ctx.run_until_cancelled(async { 7 }).await;
        assert_eq!(out.expect("detached context starts active"), 7);
    }

    #[cfg(feature = "test-util")]
    #[tokio::test]
    async fn detached_cancelled_context_short_circuits() {
        let ctx = TaskContext::detached_cancelled();

        assert!(ctx.is_cancelled());
        let out = ctx.run_until_cancelled(async { 7 }).await;
        assert!(
            matches!(out, Err(TaskError::Canceled)),
            "an already-cancelled context must win the race"
        );
    }

    #[tokio::test]
    async fn run_until_cancelled_returns_output_when_future_completes_first() {
        let ctx = TaskContext::from_token(CancellationToken::new());

        let result = ctx.run_until_cancelled(async { 7 }).await;

        assert_eq!(
            result.expect("future completed without cancellation, must yield Ok"),
            7,
            "the future's output must pass through unchanged"
        );
    }

    #[tokio::test]
    async fn run_until_cancelled_returns_canceled_when_cancelled_mid_flight() {
        let token = CancellationToken::new();
        let ctx = TaskContext::from_token(token.clone());

        let running =
            tokio::spawn(
                async move { ctx.run_until_cancelled(std::future::pending::<()>()).await },
            );
        token.cancel();

        let result = tokio::time::timeout(Duration::from_secs(1), running)
            .await
            .expect("run_until_cancelled must resolve promptly after cancellation")
            .expect("the spawned task must not panic");
        assert!(
            matches!(result, Err(TaskError::Canceled)),
            "cancellation mid-flight must yield Err(TaskError::Canceled), got {result:?}"
        );
    }

    #[tokio::test]
    async fn run_until_cancelled_does_not_poll_future_when_already_cancelled() {
        let token = CancellationToken::new();
        let ctx = TaskContext::from_token(token.clone());
        token.cancel();

        let polled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&polled);
        let result = ctx
            .run_until_cancelled(async move {
                flag.store(true, Ordering::SeqCst);
                7
            })
            .await;

        assert!(
            matches!(result, Err(TaskError::Canceled)),
            "an already-cancelled context must yield Err(TaskError::Canceled), got {result:?}"
        );
        assert!(
            !polled.load(Ordering::SeqCst),
            "the future must not be polled when the context is already cancelled (cancellation wins ties)"
        );
    }

    #[test]
    fn context_and_clone_share_underlying_cancellation_state() {
        let token = CancellationToken::new();
        let ctx = TaskContext::from_token(token.clone());
        let clone = ctx.clone();

        assert!(!ctx.is_cancelled(), "fresh context must not be cancelled");
        assert!(!clone.is_cancelled(), "fresh clone must not be cancelled");
        token.cancel();
        assert!(
            ctx.is_cancelled(),
            "context must observe the underlying token's cancellation"
        );
        assert!(
            clone.is_cancelled(),
            "a cloned context must share cancellation state with the original"
        );
    }

    #[tokio::test]
    async fn cancelled_resolves_once_token_is_cancelled() {
        let token = CancellationToken::new();
        let ctx = TaskContext::from_token(token.clone());
        token.cancel();

        tokio::time::timeout(Duration::from_secs(1), ctx.cancelled())
            .await
            .expect("cancelled() must resolve promptly after the token is cancelled");
    }

    #[test]
    fn child_is_cancelled_when_parent_cancels() {
        let token = CancellationToken::new();
        let ctx = TaskContext::from_token(token.clone());
        let child = ctx.child();

        assert!(!child.is_cancelled(), "fresh child must not be cancelled");
        token.cancel();
        assert!(
            child.is_cancelled(),
            "cancelling the parent must propagate to a child context"
        );
    }

    #[cfg(feature = "tokio-util-interop")]
    #[test]
    fn cancellation_token_shares_state_with_context() {
        let token = CancellationToken::new();
        let ctx = TaskContext::from_token(token.clone());
        let raw = ctx.cancellation_token();

        assert!(!raw.is_cancelled());
        token.cancel();
        assert!(
            raw.is_cancelled(),
            "the raw token must share cancellation state with the context"
        );
    }

    #[cfg(feature = "tokio-util-interop")]
    #[test]
    fn child_cancellation_does_not_affect_parent() {
        let ctx = TaskContext::from_token(CancellationToken::new());
        let child = ctx.child();

        child.cancellation_token().cancel();
        assert!(
            child.is_cancelled(),
            "child must observe its own cancellation"
        );
        assert!(
            !ctx.is_cancelled(),
            "cancelling a child must not cancel the parent (child() must use a child token, not a clone)"
        );
    }
}
