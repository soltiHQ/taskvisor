//! Cooperative cancellation for one task attempt.

use std::future::Future;

use tokio_util::sync::CancellationToken;

use crate::error::TaskError;

/// Cancellation context passed to one [`Task`](crate::Task) attempt.
///
/// ## Cancellation
///
/// The supervisor cancels this context when the task is removed or the runtime
/// shuts down. Cancellation is a signal; it does not stop user code by itself.
/// Long-running tasks must observe the signal and return.
///
/// ```text
/// remove / cancel / shutdown
///             |
///             v
///       TaskContext cancelled
///             |
///             v
/// task returns TaskError::Canceled
/// ```
///
/// Await [`cancelled`](Self::cancelled), check
/// [`is_cancelled`](Self::is_cancelled), or wrap a cancellation-safe future in
/// [`run_until_cancelled`](Self::run_until_cancelled).
#[derive(Clone, Debug)]
pub struct TaskContext {
    cancel: CancellationToken,
}

impl TaskContext {
    /// Wraps the token created by the runtime.
    pub(crate) fn from_token(cancel: CancellationToken) -> Self {
        Self { cancel }
    }

    /// Creates an active context that is not connected to a supervisor.
    ///
    /// Use it in tests that call [`Task::spawn`](crate::Task::spawn) or another
    /// function that accepts a context.
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

    /// Creates an already-cancelled context for tests.
    ///
    /// Use it to check cancellation paths:
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

    /// Waits for cancellation.
    ///
    /// It returns immediately if cancellation has already happened. It is safe
    /// to call more than once and to use inside `tokio::select!`.
    pub async fn cancelled(&self) {
        self.cancel.cancelled().await;
    }

    /// Returns `true` after cancellation has happened.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Runs `fut` until it completes or the context is cancelled.
    ///
    /// Returns `Ok(output)` if `fut` finishes first. Returns
    /// [`TaskError::Canceled`] if cancellation wins.
    ///
    /// Cancellation wins a tie. If the context is already cancelled, `fut` is
    /// not polled. When cancellation wins, `fut` is dropped. Use this method only
    /// with futures that are safe to cancel by dropping.
    ///
    /// This is a short form of `tokio::select!` for common worker loops:
    ///
    /// ```rust,no_run
    /// use std::time::Duration;
    /// use taskvisor::{TaskFn, TaskRef};
    ///
    /// let poller: TaskRef = TaskFn::arc("poller", |ctx| async move {
    ///     loop {
    ///         ctx.run_until_cancelled(tokio::time::sleep(Duration::from_secs(5)))
    ///             .await?;
    ///         // Do one unit of work.
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

    /// Creates a child cancellation scope.
    ///
    /// Parent cancellation reaches the child. Cancelling the child through an
    /// interop API does not cancel the parent.
    #[must_use]
    pub fn child(&self) -> TaskContext {
        TaskContext {
            cancel: self.cancel.child_token(),
        }
    }

    /// Returns the underlying [`tokio_util`] cancellation token.
    ///
    /// Use this only when another API requires a `CancellationToken`. The
    /// returned token shares state with this context.
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
