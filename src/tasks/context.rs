//! Task execution context handed to [`Task::spawn`](crate::Task::spawn).

use tokio_util::sync::CancellationToken;

/// Execution context passed to a [`Task`](crate::Task) on every attempt.
///
/// Carries the per-attempt cancellation signal and is the only argument to [`Task::spawn`](crate::Task::spawn).
///
/// ## Cancellation
///
/// The context is cancelled by the supervisor during shutdown or when the task is removed at runtime.
/// - Long-running tasks should await [`cancelled`](Self::cancelled) (or poll [`is_cancelled`](Self::is_cancelled));
///   tasks that ignore it block graceful shutdown until the grace period expires.
/// - Short-lived, one-shot tasks that finish quickly may ignore it.
#[derive(Clone, Debug)]
pub struct TaskContext {
    cancel: CancellationToken,
}

impl TaskContext {
    /// Wraps a raw cancellation token (crate-internal; the runtime owns token creation).
    pub(crate) fn from_token(cancel: CancellationToken) -> Self {
        Self { cancel }
    }

    /// Resolves once the context is cancelled.
    ///
    /// Safe to call repeatedly and to use as a branch in `tokio::select!`.
    pub async fn cancelled(&self) {
        self.cancel.cancelled().await;
    }

    /// Returns `true` if the context has already been cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Derives a child context: cancelled when this context is cancelled, but cancelling the child does **not** affect this context.
    #[must_use]
    pub fn child(&self) -> TaskContext {
        TaskContext {
            cancel: self.cancel.child_token(),
        }
    }

    /// Returns a clone of the underlying [`tokio_util`] cancellation token.
    ///
    /// Escape hatch for interop with third-party `tokio-util`-aware APIs. The
    /// returned token shares cancellation state with this context.
    ///
    /// Requires the `tokio-util-interop` feature.
    /// **Enabling it makes `tokio-util` a public dependency of your crate** (its semver leaks into yours);
    /// prefer [`cancelled`](Self::cancelled)/[`child`](Self::child) where possible.
    #[cfg(feature = "tokio-util-interop")]
    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancel.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::TaskContext;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn is_cancelled_reflects_underlying_token() {
        let token = CancellationToken::new();
        let ctx = TaskContext::from_token(token.clone());

        assert!(!ctx.is_cancelled(), "fresh context must not be cancelled");
        token.cancel();
        assert!(
            ctx.is_cancelled(),
            "context must observe the underlying token's cancellation"
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
    fn clone_shares_cancellation_state() {
        let token = CancellationToken::new();
        let ctx = TaskContext::from_token(token.clone());
        let clone = ctx.clone();

        token.cancel();
        assert!(
            clone.is_cancelled(),
            "a cloned context must share cancellation state with the original"
        );
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
