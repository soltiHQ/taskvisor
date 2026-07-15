//! Management and control-plane messages accepted by the registry.

use std::sync::Arc;

use tokio::sync::oneshot;

use super::completion::{OutcomeTx, RemovalCompletion};
use crate::{error::RuntimeError, identity::TaskId, tasks::TaskSpec};

/// Authoritative result of one single-task or batch registry add command.
pub(crate) type AddReply = Result<(), RuntimeError>;

/// Receiver for an authoritative registry add result.
pub(crate) type AddReplyRx = oneshot::Receiver<AddReply>;

/// One task owned by the atomic static-run registration command.
pub(crate) struct AddBatchItem {
    pub(crate) id: TaskId,
    pub(crate) label: Arc<str>,
    pub(crate) spec: TaskSpec,
}

/// Authoritative result of one registry remove command.
///
/// `Ok(true)` means the registry claimed the task and sent cancellation.
/// > It does not mean the actor has terminated yet.
pub(crate) type RemoveReply = Result<bool, RuntimeError>;

/// Receiver for an authoritative registry remove result.
pub(crate) type RemoveReplyRx = oneshot::Receiver<RemoveReply>;

/// Registry decision returned to one cancellation caller.
///
/// `claimed` is true only for the caller that changed `Registered` to `Removing`.
/// > Every caller that observes the same removal waits on the same terminal completion.
pub(crate) struct CancelDecision {
    pub(crate) id: TaskId,
    pub(crate) claimed: bool,
    pub(super) completion: RemovalCompletion,
}

impl CancelDecision {
    /// Waits until the actor is joined or force-aborted and terminal cleanup is committed.
    pub(crate) async fn wait(&self) {
        self.completion.wait().await;
    }

    /// Returns true when terminal cleanup has already been committed.
    pub(crate) fn is_complete(&self) -> bool {
        self.completion.is_complete()
    }
}

/// Authoritative result of one registry cancel command.
///
/// `Ok(None)` means no registry entry exists at this command's ordering point: the identity is unknown or terminal cleanup has already removed it.
pub(crate) type CancelReply = Result<Option<CancelDecision>, RuntimeError>;

/// Receiver for an authoritative registry cancel decision.
pub(crate) type CancelReplyRx = oneshot::Receiver<CancelReply>;

/// Command sent to the registry over the management channel.
pub(crate) enum RegistryCommand {
    /// Register a task under a pre-minted runtime identity.
    Add {
        id: TaskId,
        spec: TaskSpec,
        outcome: Option<OutcomeTx>,
        completion: Option<RemovalCompletion>,
        reply: oneshot::Sender<AddReply>,
    },
    /// Validate and register every static-run task as one operation.
    AddBatch {
        items: Vec<AddBatchItem>,
        reply: oneshot::Sender<AddReply>,
    },
    /// Remove a task by runtime identity.
    ///
    /// The identity caller publishes `TaskRemoveRequested` before sending this.
    Remove {
        id: TaskId,
        reply: oneshot::Sender<RemoveReply>,
    },
    /// Resolve a label and claim its current owner in one registry operation.
    ///
    /// The registry publishes `TaskRemoveRequested` with the resolved identity before it attempts the state transition.
    RemoveByLabel {
        label: Arc<str>,
        reply: oneshot::Sender<RemoveReply>,
    },
    /// Claim or join cancellation by runtime identity.
    Cancel {
        id: TaskId,
        reply: oneshot::Sender<CancelReply>,
    },
    /// Resolve a label and claim or join its cancellation atomically.
    CancelByLabel {
        label: Arc<str>,
        reply: oneshot::Sender<CancelReply>,
    },
}

/// Reliable control messages that must not wait for management queue capacity.
pub(super) enum RegistryControl {
    /// Confirms that every command committed before admission closed reached its direct registry decision.
    Fence { reply: oneshot::Sender<()> },
}
