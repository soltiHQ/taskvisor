//! Cloneable command-side handle for controller submissions and identity operations.

use tokio::sync::{mpsc, oneshot};

use crate::{
    RuntimeError, TaskOutcome,
    controller::{ControllerError, ControllerSpec},
    identity::TaskId,
};

use super::{ControllerCommand, IdentityOperation, Submission};

#[derive(Clone)]
pub(crate) struct ControllerHandle {
    tx: mpsc::Sender<ControllerCommand>,
}

impl ControllerHandle {
    pub(super) fn new(tx: mpsc::Sender<ControllerCommand>) -> Self {
        Self { tx }
    }

    /// Sends a submission to the ordered controller command channel.
    ///
    /// This waits for command-channel capacity.
    /// `Ok(id)` means the controller received the submission.
    /// It does not mean the task has been admitted to the runtime yet.
    pub async fn submit(&self, spec: ControllerSpec) -> Result<TaskId, ControllerError> {
        let id = TaskId::next();
        self.tx
            .send(ControllerCommand::Submit(Submission {
                id,
                spec,
                done: None,
            }))
            .await
            .map_err(|_| ControllerError::Closed)?;
        Ok(id)
    }

    /// Tries to send a submission without waiting for command-channel capacity.
    ///
    /// `ControllerError::Full` means the controller command channel is full.
    /// It does not mean the target slot queue is full.
    pub fn try_submit(&self, spec: ControllerSpec) -> Result<TaskId, ControllerError> {
        let id = TaskId::next();
        self.tx
            .try_send(ControllerCommand::Submit(Submission {
                id,
                spec,
                done: None,
            }))
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => ControllerError::Full,
                mpsc::error::TrySendError::Closed(_) => ControllerError::Closed,
            })?;
        Ok(id)
    }

    /// Sends a watched submission to the ordered controller command channel.
    ///
    /// The returned receiver resolves to:
    /// - `TaskOutcome::Rejected` if the controller never admits the task body,
    /// - the runtime task outcome if the task is admitted and later terminates.
    ///
    /// `Ok((id, rx))` means the controller received the submission.
    /// It does not mean the slot accepted it yet.
    pub async fn submit_and_watch(
        &self,
        spec: ControllerSpec,
    ) -> Result<(TaskId, oneshot::Receiver<TaskOutcome>), ControllerError> {
        let id = TaskId::next();
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ControllerCommand::Submit(Submission {
                id,
                spec,
                done: Some(tx),
            }))
            .await
            .map_err(|_| ControllerError::Closed)?;
        Ok((id, rx))
    }

    /// Sends one waiting identity operation to the ordered controller command channel.
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

    /// Sends one fail-fast identity operation to the ordered controller command channel.
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
                mpsc::error::TrySendError::Full(_) => RuntimeError::CommandQueueFull,
                mpsc::error::TrySendError::Closed(_) => RuntimeError::ShuttingDown,
            })?;
        reply_rx.await.map_err(|_| RuntimeError::ShuttingDown)?
    }

    pub(crate) async fn remove(&self, id: TaskId) -> Result<bool, RuntimeError> {
        self.manage_identity(id, IdentityOperation::Remove).await
    }

    pub(crate) async fn try_remove(&self, id: TaskId) -> Result<bool, RuntimeError> {
        self.try_manage_identity(id, IdentityOperation::TryRemove)
            .await
    }

    pub(crate) async fn cancel(&self, id: TaskId) -> Result<bool, RuntimeError> {
        self.manage_identity(id, IdentityOperation::Cancel).await
    }

    pub(crate) async fn cancel_with_timeout(
        &self,
        id: TaskId,
        wait_for: std::time::Duration,
    ) -> Result<bool, RuntimeError> {
        self.manage_identity(id, IdentityOperation::CancelWithTimeout(wait_for))
            .await
    }
}
