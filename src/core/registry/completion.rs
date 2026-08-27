//! Signals registry cleanup and physical actor release for one removal.
//!
//! Natural actor exit, remove, cancel, and shutdown all enter the same removal path.
//! [`RemovalCompletion`] lets other components observe that path without owning the actor join.
//! Public cancellation waits for logical completion.
//! The controller also waits for physical completion before it reuses a slot.
//!
//! ```text
//! registered ───────────► removing ──────────────────────────► membership and reporting ──► logical
//! force-aborted actor ──► physical exit and terminal match ──► physical
//! ```
//!
//! Logical completion means membership was removed and terminal reporting was attempted.
//! Physical completion means the actor has exited and force-abort tracking has ended.
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
    /// Wakes controller replacement after the physical attempt is fully released.
    physical: CancellationToken,
}

impl RemovalCompletion {
    /// New incomplete logical and physical completion pair.
    pub(crate) fn new() -> Self {
        Self {
            logical: CancellationToken::new(),
            physical: CancellationToken::new(),
        }
    }

    /// Logical completion without ownership of the removal operation.
    pub(crate) async fn wait(&self) {
        self.logical.cancelled().await;
    }

    /// Logical and physical completion for controller slot reuse.
    #[cfg(feature = "controller")]
    pub(crate) async fn wait_physical(&self) {
        self.logical.cancelled().await;
        self.physical.cancelled().await;
    }

    /// Whether terminal registry cleanup has been committed.
    pub(super) fn is_complete(&self) -> bool {
        self.logical.is_cancelled()
    }

    #[cfg(test)]
    /// Returns whether the physical attempt has been fully released.
    pub(super) fn is_physical_complete(&self) -> bool {
        self.physical.is_cancelled()
    }

    /// Bounded logical terminal transition marked complete.
    pub(super) fn complete_logical(&self) {
        self.logical.cancel();
    }

    /// Physical attempt marked fully released.
    pub(super) fn complete_physical(&self) {
        self.physical.cancel();
    }

    /// Whether both values observe the same physical-release latch.
    pub(super) fn shares_physical_latch(&self, other: &Self) -> bool {
        self.physical == other.physical
    }
}
