//! Signals registry cleanup and physical actor release for one removal.
//!
//! Natural actor exit, remove, cancel, and shutdown all enter the same removal path.
//! [`RemovalCompletion`] lets other components observe that path without owning the actor join.
//! Public cancellation waits for logical completion. The controller also waits for physical completion before it reuses a slot.
//!
//! ```text
//! registered ───────────► removing ──────────────────────────► membership and reporting ──► logical
//! force-aborted actor ──► physical exit and terminal match ──► physical
//! ```
//!
//! Logical completion means membership was removed and terminal reporting was attempted.
//! Physical completion means the actor or reaper no longer owns the attempt.
//! These latches do not request cancellation.

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::core::outcome::TaskOutcome;

/// Sender used to resolve a watched task with its final [`TaskOutcome`].
pub(crate) type OutcomeTx = oneshot::Sender<TaskOutcome>;

/// Shared two-phase completion for one registry removal.
#[derive(Clone, Debug)]
pub(crate) struct RemovalCompletion {
    /// Wakes public cancellation waiters after terminal commit.
    logical: CancellationToken,
    /// Wakes controller replacement after actor and reaper ownership is released.
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

    /// Waits for logical completion without owning the removal operation.
    pub(crate) async fn wait(&self) {
        self.logical.cancelled().await;
    }

    /// Waits for both logical and physical completion.
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
    /// Returns whether actor and reaper ownership has been released.
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
