//! Messages that cross the controller loop boundary.
//!
//! [`ControllerHandle`](super::ControllerHandle) sends [`ControllerCommand`] values through the ordered command queue.
//! Tracked runtime operations return the result types in this module to the same loop.
//! Task and slot identities let the loop discard results that no longer match current state.

use std::sync::Arc;

use tokio::sync::oneshot;

use crate::{
    RuntimeError,
    controller::spec::ControllerSpec,
    core::{OutcomeTx, RemovalCompletion, deferred_drop::OwnedTask},
    identity::TaskId,
};

/// Ordered command accepted by the controller channel.
pub(super) enum ControllerCommand {
    /// Apply admission policy for one new submission.
    Submit(Box<Submission>),
    /// Start one identity operation after earlier controller commands are handled.
    ManageIdentity {
        /// Task identity to manage.
        id: TaskId,
        /// Operation selected by the caller.
        operation: IdentityOperation,
        /// Result channel retained by the engine.
        reply: oneshot::Sender<Result<bool, RuntimeError>>,
    },
}

/// Identity operation owned by one accepted controller command.
#[derive(Clone, Copy, Debug)]
pub(super) enum IdentityOperation {
    /// Remove the task, waiting for registry command capacity when needed.
    Remove,
    /// Remove the task without waiting for registry command capacity.
    TryRemove,
    /// Cancel the task, waiting for registry command capacity when needed.
    Cancel,
    /// Cancel the task without waiting for registry command capacity.
    TryCancel,
    /// Cancel the task and bound runtime cleanup waiting.
    CancelWithTimeout(std::time::Duration),
    /// Cancel without waiting for registry capacity, then bound cleanup waiting.
    TryCancelWithTimeout(std::time::Duration),
}

impl IdentityOperation {
    /// Optional reason carried by `TaskRemoveRequested` when queued work is removed.
    pub(super) fn request_reason(self) -> Option<&'static str> {
        match self {
            Self::Remove | Self::TryRemove => None,
            Self::Cancel
            | Self::TryCancel
            | Self::CancelWithTimeout(_)
            | Self::TryCancelWithTimeout(_) => Some("manual_cancel"),
        }
    }
}

/// Resolves an accepted identity caller if its tracked operation is dropped.
pub(super) struct IdentityReply {
    /// Reply sender consumed by explicit delivery or shutdown fallback.
    sender: Option<oneshot::Sender<Result<bool, RuntimeError>>>,
}

impl IdentityReply {
    /// Wraps an accepted caller reply.
    pub(super) fn new(sender: oneshot::Sender<Result<bool, RuntimeError>>) -> Self {
        Self {
            sender: Some(sender),
        }
    }

    /// Sends the operation result once.
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
    /// Preallocated identity used for events, slot state, and outcome correlation.
    pub(super) id: TaskId,
    /// Admission policy and task spec coupled to pre-reserved destructor ownership.
    pub(super) owned: OwnedTask<ControllerSpec>,
    /// Optional watched-outcome sender for `submit_and_watch`.
    pub(super) done: Option<OutcomeTx>,
}

/// Authoritative registry decision for one in-flight slot admission.
pub(super) struct AdmissionResult {
    /// Preallocated identity used to reject stale results safely.
    pub(super) id: TaskId,
    /// Slot recorded when admission tracking started.
    pub(super) slot_name: Arc<str>,
    /// Authoritative result of the committed registration command.
    pub(super) decision: Result<RemovalCompletion, RuntimeError>,
}

/// Physical release of one admitted slot owner.
pub(super) struct CompletionResult {
    /// Runtime identity whose physical actor and reaper ownership was released.
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
    /// Direct registry claim decision or registry-operation failure.
    pub(super) decision: Result<bool, RuntimeError>,
}
