//! # Registry completion signals
//!
//! Natural completion, an explicit remove or cancel, and shutdown can all start
//! terminal removal. One path claims the actor and becomes its join owner.
//! [`RemovalCompletion`] connects callers waiting for cleanup with that owner.
//!
//! ```text
//! Registered ──► Removing ──► logical terminal ──► remove registry membership
//!                                  │                         │
//!                                  ▼                         ▼
//!                         public completion       physical actor/reaper release
//! ```

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::core::outcome::TaskOutcome;

/// Sender used to resolve a watched task with its final [`TaskOutcome`].
pub(crate) type OutcomeTx = oneshot::Sender<TaskOutcome>;

/// Shared logical and physical terminal-cleanup signals.
///
/// Every clone observes the same two phases. Public cancellation and shutdown
/// wait for logical completion, which is bounded by the configured grace.
/// Controller slot reuse waits for physical completion so a logically aborted
/// but still-running actor cannot overlap its replacement.
///
/// This is not the actor's cancellation token. Creating or waiting on this value
/// does not request task cancellation.
#[derive(Clone, Debug)]
pub(crate) struct RemovalCompletion {
    logical: CancellationToken,
    physical: CancellationToken,
}

impl RemovalCompletion {
    /// Creates a new incomplete terminal-cleanup signal.
    pub(crate) fn new() -> Self {
        Self {
            logical: CancellationToken::new(),
            physical: CancellationToken::new(),
        }
    }

    /// Waits until terminal registry cleanup has been committed.
    ///
    /// If cleanup is already complete, this returns immediately. Dropping this
    /// wait does not affect other waiters or the cleanup owner.
    pub(crate) async fn wait(&self) {
        self.logical.cancelled().await;
    }

    /// Waits until the physical actor owner and terminal reaper record have
    /// both been released, and until logical terminal reporting is complete.
    #[cfg(feature = "controller")]
    pub(crate) async fn wait_physical(&self) {
        self.logical.cancelled().await;
        self.physical.cancelled().await;
    }

    /// Returns `true` when terminal registry cleanup has been committed.
    pub(super) fn is_complete(&self) -> bool {
        self.logical.is_cancelled()
    }

    #[cfg(test)]
    pub(super) fn is_physical_complete(&self) -> bool {
        self.physical.is_cancelled()
    }

    /// Marks the bounded logical terminal transition complete.
    pub(super) fn complete_logical(&self) {
        self.logical.cancel();
    }

    /// Marks the physical actor/reaper ownership transition complete.
    pub(super) fn complete_physical(&self) {
        self.physical.cancel();
    }

    /// Returns whether both values observe the same physical-release latch.
    pub(super) fn shares_physical_latch(&self, other: &Self) -> bool {
        self.physical == other.physical
    }
}
