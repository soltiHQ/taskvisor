//! Commands and completion messages exchanged by the controller engine.

use std::sync::Arc;

use tokio::sync::oneshot;

use crate::{
    RuntimeError,
    controller::spec::ControllerSpec,
    core::{OutcomeTx, RemovalCompletion},
    identity::TaskId,
};

pub(super) enum ControllerCommand {
    /// Apply admission policy for one new submission.
    Submit(Submission),
    /// Apply one identity operation after all earlier controller submissions.
    ///
    /// The controller removes queued work itself and owns registry fallback for every other id.
    ManageIdentity {
        id: TaskId,
        operation: IdentityOperation,
        reply: oneshot::Sender<Result<bool, RuntimeError>>,
    },
}

/// Identity operation owned by one accepted controller command.
#[derive(Clone, Copy, Debug)]
pub(super) enum IdentityOperation {
    Remove,
    TryRemove,
    Cancel,
    CancelWithTimeout(std::time::Duration),
}

impl IdentityOperation {
    /// Optional reason carried by `TaskRemoveRequested` when queued work is removed.
    pub(super) fn request_reason(self) -> Option<&'static str> {
        match self {
            Self::Remove | Self::TryRemove => None,
            Self::Cancel | Self::CancelWithTimeout(_) => Some("manual_cancel"),
        }
    }
}

/// Ensures an accepted identity caller receives an explicit shutdown result if its worker aborts.
pub(super) struct IdentityReply {
    sender: Option<oneshot::Sender<Result<bool, RuntimeError>>>,
}

impl IdentityReply {
    pub(super) fn new(sender: oneshot::Sender<Result<bool, RuntimeError>>) -> Self {
        Self {
            sender: Some(sender),
        }
    }

    pub(super) fn send(mut self, result: Result<bool, RuntimeError>) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(result);
        }
    }
}

impl Drop for IdentityReply {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(Err(RuntimeError::ShuttingDown));
        }
    }
}

/// Submission accepted by the controller command channel.
pub(super) struct Submission {
    /// Pre-minted identity used for events, slot state, and final outcome correlation.
    pub(super) id: TaskId,
    /// Admission policy, task spec, and optional slot key.
    pub(super) spec: ControllerSpec,
    /// Optional watched-outcome sender for `submit_and_watch`.
    pub(super) done: Option<OutcomeTx>,
}

/// Authoritative registry decision for one in-flight slot admission.
pub(super) struct AdmissionResult {
    /// Pre-minted identity used to reject stale results safely.
    pub(super) id: TaskId,
    /// Slot that owned `id` when the Add command was committed.
    pub(super) slot_name: Arc<str>,
    /// Direct registry decision and the terminal signal for an accepted task.
    pub(super) decision: Result<RemovalCompletion, RuntimeError>,
}

/// Reliable registry cleanup for one admitted slot owner.
pub(super) struct CompletionResult {
    /// Runtime identity that reached terminal registry cleanup.
    pub(super) id: TaskId,
    /// Slot that owned `id` when completion tracking started.
    pub(super) slot_name: Arc<str>,
}

/// Result of ordering removal for one controller-owned runtime task.
pub(super) struct RemovalResult {
    /// Runtime identity whose removal was requested.
    pub(super) id: TaskId,
    /// Slot that owned `id` when removal tracking started.
    pub(super) slot_name: Arc<str>,
    /// Direct registry claim decision or management-plane failure.
    pub(super) decision: Result<bool, RuntimeError>,
}
