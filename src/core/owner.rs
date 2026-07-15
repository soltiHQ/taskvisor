//! # Public runtime ownership
//!
//! [`RuntimeOwner`] is the shared lease held by [`Supervisor`](crate::Supervisor) and every [`SupervisorHandle`](crate::SupervisorHandle).
//!
//! ```text
//! Supervisor ────────────┐
//! SupervisorHandle ──────┼── Arc<RuntimeOwner> ── Arc<SupervisorCore>
//! cloned handles ────────┘
//!
//! internal workers ────────────────────────────── Arc<SupervisorCore>
//! ```
//!
//! Internal workers retain the runtime core directly, but they do not retain `RuntimeOwner`.
//! The last public owner can therefore disappear while cleanup work still holds the core alive.
//!
//! Dropping that last owner cannot wait for graceful cleanup.
//! It invokes [`SupervisorCore::abandon`] to close runtime intake and propagate cancellation.
//! If an explicit shared shutdown operation already exists, that operation keeps ownership of cleanup and the fallback does not replace it.

use std::sync::Arc;

use super::SupervisorCore;

/// Shared ownership held by `Supervisor` and every management handle.
///
/// Internal workers do not hold this value.
/// Its destructor therefore runs when the last public owner disappears and starts best-effort runtime cancellation.
pub(crate) struct RuntimeOwner {
    /// Runtime state shared with public owners and internal workers.
    ///
    /// Keeping the core behind a separate `Arc` lets detached shutdown and cleanup work outlive the public ownership lease when necessary.
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

/// Omits runtime internals from the ownership lease's debug representation.
impl std::fmt::Debug for RuntimeOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeOwner").finish_non_exhaustive()
    }
}

/// Starts the non-blocking last-owner fallback.
///
/// `Drop` cannot await joins or return a shutdown result.
/// Confirmed cleanup must go through [`SupervisorHandle::shutdown`](crate::SupervisorHandle::shutdown).
impl Drop for RuntimeOwner {
    fn drop(&mut self) {
        self.core.abandon();
    }
}
