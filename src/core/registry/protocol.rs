//! Defines messages between runtime management and the registry.
//!
//! [`SupervisorCore`](crate::core::runtime::SupervisorCore) commits ordered management commands to a bounded queue.
//! The registry listener returns its decision through the command's one-shot sender. These direct replies are the
//! source of truth for add, remove, and cancel calls. Lifecycle events are not part of the reply path.
//!
//! [`RegistryControl`] uses a separate channel.
//! Its fence can drain commands that passed shutdown admission without waiting for management queue capacity.

use std::sync::Arc;

use tokio::sync::oneshot;

use super::completion::{OutcomeTx, RemovalCompletion};
use crate::{
    core::deferred_drop::OwnedTask, error::RuntimeError, identity::TaskId, tasks::TaskSpec,
};

/// Authoritative result of one single-task or batch registry add command.
pub(crate) type AddReply = Result<(), RuntimeError>;

/// Receiver for an authoritative registry add result.
pub(crate) type AddReplyRx = oneshot::Receiver<AddReply>;

/// One task owned by the atomic static-run registration command.
pub(crate) struct AddBatchItem {
    pub(crate) id: TaskId,
    pub(crate) label: Arc<str>,
    pub(crate) owned: OwnedTask<TaskSpec>,
}

/// Authoritative result of one registry remove command.
///
/// `Ok(true)` means the command claimed the task and sent cancellation.
/// The actor may still be running and membership remains until terminal cleanup.
pub(crate) type RemoveReply = Result<bool, RuntimeError>;

/// Receiver for an authoritative registry remove result.
pub(crate) type RemoveReplyRx = oneshot::Receiver<RemoveReply>;

/// Registry decision returned to one cancellation caller.
///
/// `claimed` is true only when this command changed `Registered` to `Removing`.
/// Later commands for that entry receive the same logical completion latch.
pub(crate) struct CancelDecision {
    /// Identity resolved at the command ordering point.
    pub(crate) id: TaskId,
    /// Whether this command started the single join owner.
    pub(crate) claimed: bool,
    /// Logical removal latch shared by all cancellation callers.
    pub(super) completion: RemovalCompletion,
}

impl CancelDecision {
    /// Waits until terminal membership and reporting are committed.
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
/// `Ok(None)` means no entry exists at this command's ordering point.
/// The identity is unknown or terminal cleanup already removed it.
pub(crate) type CancelReply = Result<Option<CancelDecision>, RuntimeError>;

/// Receiver for an authoritative registry cancel decision.
pub(crate) type CancelReplyRx = oneshot::Receiver<CancelReply>;

/// Command sent to the registry over the management channel.
pub(crate) enum RegistryCommand {
    /// Register one task under an assigned runtime identity.
    Add {
        id: TaskId,
        label: Arc<str>,
        /// Keeps destructor capacity reserved after command handoff.
        owned: Box<OwnedTask<TaskSpec>>,
        /// Direct path for a watched terminal or rejected outcome.
        outcome: Option<OutcomeTx>,
        /// Lets controller admission track physical release when present.
        completion: Option<RemovalCompletion>,
        /// Returns the decision before the actor start gate opens.
        reply: oneshot::Sender<AddReply>,
    },
    /// Validate and register every static-run task as one operation.
    AddBatch {
        items: Vec<AddBatchItem>,
        /// Returns the decision before the shared start gate opens.
        reply: oneshot::Sender<AddReply>,
    },
    /// Remove a task by runtime identity.
    ///
    /// The identity caller publishes `TaskRemoveRequested` before sending this.
    Remove {
        id: TaskId,
        /// Reports the claim without waiting for terminal cleanup.
        reply: oneshot::Sender<RemoveReply>,
    },
    /// Resolve a label and claim its current owner in one registry operation.
    ///
    /// The registry claims under the state lock, then publishes `TaskRemoveRequested` with the resolved identity.
    RemoveByLabel {
        label: Arc<str>,
        /// Reports the claim without waiting for terminal cleanup.
        reply: oneshot::Sender<RemoveReply>,
    },
    /// Start cancellation or join an existing removal by runtime identity.
    Cancel {
        id: TaskId,
        /// Returns the resolved identity, claim result, and logical latch.
        reply: oneshot::Sender<CancelReply>,
    },
    /// Resolve a label and start or join its cancellation atomically.
    CancelByLabel {
        label: Arc<str>,
        /// Returns the resolved identity, claim result, and logical latch.
        reply: oneshot::Sender<CancelReply>,
    },
}

/// Control messages carried outside the bounded management queue.
pub(super) enum RegistryControl {
    /// Confirms decisions for commands committed before admission closed.
    Fence {
        reply: oneshot::Sender<()>,
    },
}
