//! Describes one supervisor's retained user-value ownership.
//!
//! [`OwnershipSnapshot`] combines admission-capacity state with deferred-cleanup queue state.
//! It is separate from task membership and active-attempt queries:
//! a task can leave both views while its final user-owned values are still queued or running on destructor-isolation workers.

/// Point-in-time ownership-admission and deferred-cleanup state.
///
/// Finite capacity is shared by accepted tasks and configured subscribers.
/// A unit remains in use until its final user-owned values finish isolated destruction.
/// Destructor failure can permanently reduce the effective limit.
///
/// The three limit fields are `None` when ownership admission is configured as unlimited.
/// Cleanup counts remain available in that mode.
///
/// Configured, effective, available, and waiter values are copied under one broker lock.
/// `admission_open` also combines a separate runtime-lifecycle read, and cleanup values are copied under the worker lock.
/// A concurrent transition can therefore appear in only one part of this rolling snapshot.
/// The complete value can also become stale immediately after it is returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct OwnershipSnapshot {
    /// Original finite ownership limit, or `None` for unlimited admission.
    pub configured_limit: Option<usize>,
    /// Usable finite limit after permanent retirement, or `None` for unlimited admission.
    pub effective_limit: Option<usize>,
    /// Finite units not held by permits or grants, or `None` for unlimited admission.
    pub available: Option<usize>,
    /// Requests currently parked in finite-capacity admission.
    ///
    /// A complete grant that has not yet been observed is already counted as in use, not as a waiter.
    pub waiters: usize,
    /// Whether the runtime and ownership broker still accept new requests.
    ///
    /// An open snapshot may have no currently available units.
    pub admission_open: bool,
    /// Deferred-cleanup batches waiting for a destructor-isolation worker.
    pub cleanup_queued: usize,
    /// Deferred-cleanup batches claimed by workers and not yet completed.
    pub cleanup_running: usize,
}

impl OwnershipSnapshot {
    /// Creates one snapshot from internal broker and worker state.
    pub(crate) const fn new(
        configured_limit: Option<usize>,
        effective_limit: Option<usize>,
        available: Option<usize>,
        waiters: usize,
        admission_open: bool,
        cleanup_queued: usize,
        cleanup_running: usize,
    ) -> Self {
        Self {
            configured_limit,
            effective_limit,
            available,
            waiters,
            admission_open,
            cleanup_queued,
            cleanup_running,
        }
    }

    /// Units permanently removed from a finite configured limit.
    ///
    /// Unlimited admission produces `None`.
    #[must_use]
    pub fn retired(&self) -> Option<usize> {
        self.configured_limit
            .zip(self.effective_limit)
            .map(|(configured, effective)| configured.saturating_sub(effective))
    }

    /// Finite units currently held by permits or unobserved grants.
    ///
    /// This includes accepted task and subscriber ownership as well as queued or running deferred cleanup.
    /// Unlimited admission produces `None`.
    #[must_use]
    pub fn in_use(&self) -> Option<usize> {
        self.effective_limit
            .zip(self.available)
            .map(|(effective, available)| effective.saturating_sub(available))
    }
}

#[cfg(test)]
mod tests {
    use super::OwnershipSnapshot;

    #[test]
    fn finite_derived_counts_separate_retired_and_in_use_units() {
        let snapshot = OwnershipSnapshot::new(Some(10), Some(8), Some(3), 2, true, 1, 2);

        assert_eq!(snapshot.retired(), Some(2));
        assert_eq!(snapshot.in_use(), Some(5));
    }

    #[test]
    fn unlimited_derived_counts_have_no_finite_meaning() {
        let snapshot = OwnershipSnapshot::new(None, None, None, 0, true, 4, 2);

        assert_eq!(snapshot.retired(), None);
        assert_eq!(snapshot.in_use(), None);
        assert_eq!(snapshot.cleanup_queued, 4);
        assert_eq!(snapshot.cleanup_running, 2);
    }
}
