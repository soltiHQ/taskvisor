//! Sends task removal and cancellation through the controller queue.
//!
//! Waiting variants apply command-queue backpressure.
//! Fail-fast variants require immediate queue capacity.
//! After intake, queued-task lookup remains ordered with submissions.
//! Other identities are handled by a bounded runtime registry operation.

use tokio::sync::oneshot;

use crate::{RuntimeError, identity::TaskId};

use super::{
    super::{ControllerCommand, IdentityOperation},
    ControllerHandle,
};

impl ControllerHandle {
    async fn manage_identity(
        &self,
        id: TaskId,
        operation: IdentityOperation,
    ) -> Result<bool, RuntimeError> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .send(ControllerCommand::ManageIdentity {
                id,
                operation,
                reply,
            })
            .await
            .map_err(|_| RuntimeError::ShuttingDown)?;
        reply_rx.await.map_err(|_| RuntimeError::ShuttingDown)?
    }

    /// Fail-fast controller intake for an identity operation.
    ///
    /// Registry fallback and runtime cleanup may still wait.
    async fn try_manage_identity(
        &self,
        id: TaskId,
        operation: IdentityOperation,
    ) -> Result<bool, RuntimeError> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .try_send(ControllerCommand::ManageIdentity {
                id,
                operation,
                reply,
            })
            .map_err(|error| match error {
                tokio::sync::mpsc::error::TrySendError::Full(_) => RuntimeError::CommandQueueFull,
                tokio::sync::mpsc::error::TrySendError::Closed(_) => RuntimeError::ShuttingDown,
            })?;
        reply_rx.await.map_err(|_| RuntimeError::ShuttingDown)?
    }

    /// Removes a queued or registered task by identity.
    pub(crate) async fn remove(&self, id: TaskId) -> Result<bool, RuntimeError> {
        self.manage_identity(id, IdentityOperation::Remove).await
    }

    /// Removes by identity without waiting for command queue capacity.
    pub(crate) async fn try_remove(&self, id: TaskId) -> Result<bool, RuntimeError> {
        self.try_manage_identity(id, IdentityOperation::TryRemove)
            .await
    }

    /// Cancels a queued or registered task by identity.
    pub(crate) async fn cancel(&self, id: TaskId) -> Result<bool, RuntimeError> {
        self.manage_identity(id, IdentityOperation::Cancel).await
    }

    /// Cancels by identity without waiting for command queue capacity.
    pub(crate) async fn try_cancel(&self, id: TaskId) -> Result<bool, RuntimeError> {
        self.try_manage_identity(id, IdentityOperation::TryCancel)
            .await
    }

    /// Cancels by identity and bounds runtime cleanup waiting.
    pub(crate) async fn cancel_with_timeout(
        &self,
        id: TaskId,
        wait_for: std::time::Duration,
    ) -> Result<bool, RuntimeError> {
        self.manage_identity(id, IdentityOperation::CancelWithTimeout(wait_for))
            .await
    }

    /// Tries controller command intake immediately, then bounds cleanup waiting.
    pub(crate) async fn try_cancel_with_timeout(
        &self,
        id: TaskId,
        wait_for: std::time::Duration,
    ) -> Result<bool, RuntimeError> {
        self.try_manage_identity(id, IdentityOperation::TryCancelWithTimeout(wait_for))
            .await
    }
}
