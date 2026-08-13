//! Tracks public ownership of one supervisor runtime.
//!
//! [`Supervisor`](crate::Supervisor) and every
//! [`SupervisorHandle`](crate::SupervisorHandle) share one [`RuntimeOwner`].
//! Internal workers hold [`SupervisorCore`] directly and do not extend the
//! public ownership lease.
//!
//! ```text
//! Supervisor ──► RuntimeOwner ──► SupervisorCore
//! handles ──► RuntimeOwner
//! workers ──► SupervisorCore
//! ```
//!
//! Dropping the last public owner calls [`SupervisorCore::abandon`]. This closes
//! admission and propagates cancellation when no shutdown operation exists.
//! If a shared shutdown operation already exists, `Drop` leaves it unchanged.

use std::sync::Arc;

use super::SupervisorCore;

/// Public ownership lease for one runtime core.
pub(crate) struct RuntimeOwner {
    /// Runtime state shared with public owners and internal workers.
    core: Arc<SupervisorCore>,
}

impl RuntimeOwner {
    /// Creates the first public ownership lease for `core`.
    ///
    /// The returned `Arc` is cloned into each [`SupervisorHandle`](crate::SupervisorHandle).
    pub(crate) fn new(core: Arc<SupervisorCore>) -> Arc<Self> {
        Arc::new(Self { core })
    }

    /// Borrows the shared runtime core without creating another public owner.
    pub(crate) fn core(&self) -> &Arc<SupervisorCore> {
        &self.core
    }
}

/// Omits runtime internals from the debug representation.
impl std::fmt::Debug for RuntimeOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeOwner").finish_non_exhaustive()
    }
}

/// Starts the non-blocking last-owner fallback.
///
/// `Drop` cannot await joins or return a shutdown result.
/// Confirmed cleanup must go through
/// [`SupervisorHandle::shutdown`](crate::SupervisorHandle::shutdown).
impl Drop for RuntimeOwner {
    fn drop(&mut self) {
        self.core.abandon();
    }
}
