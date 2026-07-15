//! # Registry completion signals
//!
//! Natural completion, an explicit remove or cancel, and shutdown can all start
//! terminal removal. One path claims the actor and becomes its join owner.
//! [`RemovalCompletion`] connects callers waiting for cleanup with that owner.
//!
//! ```text
//! Registered ──► Removing ──► join or force-abort ──► remove registry membership
//!                                                       │
//!                                                       ▼
//!                                             RemovalCompletion complete
//! ```

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::core::outcome::TaskOutcome;

/// Sender used to resolve a watched task with its final [`TaskOutcome`].
pub(crate) type OutcomeTx = oneshot::Sender<TaskOutcome>;

/// Shared one-shot signal for committed terminal registry cleanup.
///
/// Every clone observes the same completion. The signal becomes complete only
/// after the actor has been joined or force-aborted and its identity and label
/// have been removed from the registry. Watched-outcome delivery and final event
/// publication are attempted before waiters are released.
///
/// This is not the actor's cancellation token. Creating or waiting on this value
/// does not request task cancellation.
#[derive(Clone, Debug)]
pub(crate) struct RemovalCompletion {
    /// One-shot latch shared by cleanup owners and waiters.
    ///
    /// The token's cancelled state represents completed registry cleanup.
    token: CancellationToken,
}

impl RemovalCompletion {
    /// Creates a new incomplete terminal-cleanup signal.
    pub(crate) fn new() -> Self {
        Self {
            token: CancellationToken::new(),
        }
    }

    /// Waits until terminal registry cleanup has been committed.
    ///
    /// If cleanup is already complete, this returns immediately. Dropping this
    /// wait does not affect other waiters or the cleanup owner.
    pub(crate) async fn wait(&self) {
        self.token.cancelled().await;
    }

    /// Returns `true` when terminal registry cleanup has been committed.
    pub(super) fn is_complete(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Marks terminal registry cleanup complete and releases all waiters.
    ///
    /// Repeated calls leave the signal complete.
    pub(super) fn complete(&self) {
        self.token.cancel();
    }
}
