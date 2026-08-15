//! Routes rejected controller-owned values to destructor isolation.
//!
//! Admission, identity removal, and shutdown call these helpers after they finish the related controller-state transition.
//! Each task already owns a cleanup reservation. An undelivered terminal outcome stays in the same bundle.
//!
//! This module is the final controller-side ownership step for rejected work.
//! It does not change slot or queue state.

use crate::{
    RuntimeError,
    core::{TaskOutcome, deferred_drop::OwnedTask},
    events::RejectionKind,
};

use super::super::{Controller, state::PendingSubmission};

/// Intact registry handoff returned before the Add command commits.
pub(in crate::controller::engine) type StartFailure = Box<crate::core::UncommittedWatchedAdd>;

impl Controller {
    /// Sends recovered handoff failures to their reserved cleanup bundles.
    pub(in crate::controller::engine) fn drop_pending_submissions(
        &self,
        pending: Vec<(StartFailure, Option<TaskOutcome>)>,
    ) {
        for (pending, terminal) in pending {
            self.drop_start_failure(pending, terminal);
        }
    }

    /// Sends one controller-owned value and optional outcome to reserved cleanup.
    pub(in crate::controller::engine) fn dispose_owned_task<T>(
        &self,
        owned: OwnedTask<T>,
        terminal: Option<TaskOutcome>,
    ) where
        T: Send + 'static,
    {
        let (value, mut cleanup) = owned.into_parts();
        drop(value);
        if let Some(terminal) = terminal {
            cleanup.attach_outcome(terminal);
        }
        cleanup.submit();
    }

    /// Sends one rejected pending submission to reserved cleanup.
    pub(in crate::controller::engine) fn drop_pending_submission(
        &self,
        pending: PendingSubmission,
        terminal: Option<TaskOutcome>,
    ) {
        let PendingSubmission { owned, .. } = pending;
        self.dispose_owned_task(owned, terminal);
    }

    /// Sends one recovered registry handoff to reserved cleanup.
    pub(super) fn drop_start_failure(&self, pending: StartFailure, terminal: Option<TaskOutcome>) {
        let crate::core::UncommittedWatchedAdd {
            error,
            label,
            owned,
            done,
        } = *pending;
        debug_assert!(done.is_none(), "the watcher must be restored before drop");
        drop((error, label, done));
        self.dispose_owned_task(owned, terminal);
    }

    /// Classifies a runtime admission failure for controller reporting.
    pub(super) fn rejection_kind_for_runtime_error(error: &RuntimeError) -> RejectionKind {
        if matches!(error, RuntimeError::ResourceLimitReached { .. }) {
            RejectionKind::ResourceLimit
        } else {
            RejectionKind::AdmissionFailed
        }
    }
}
