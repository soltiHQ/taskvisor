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
    /// `Ok(id)` means the channel accepted the command.
    ///
    /// The controller may not have applied the admission policy yet, and runtime admission happens later.
    #[cfg(test)]
    pub async fn submit(&self, spec: ControllerSpec) -> Result<TaskId, ControllerError> {
        let id = TaskId::next();
        self.submit_prepared(id, spec).await
    }

    pub(crate) async fn submit_prepared(
        &self,
        id: TaskId,
        spec: ControllerSpec,
    ) -> Result<TaskId, ControllerError> {
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
    #[cfg(test)]
    pub fn try_submit(&self, spec: ControllerSpec) -> Result<TaskId, ControllerError> {
        let id = TaskId::next();
        self.try_submit_prepared(id, spec)
    }

    pub(crate) fn try_submit_prepared(
        &self,
        id: TaskId,
        spec: ControllerSpec,
    ) -> Result<TaskId, ControllerError> {
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
    /// - `TaskOutcome::Rejected` if registration never succeeds,
    /// - the runtime task outcome after the registry accepts the task.
    ///
    /// `Ok((id, rx))` means the channel accepted the command.
    /// The controller may not have applied the slot policy yet.
    #[cfg(test)]
    pub async fn submit_and_watch(
        &self,
        spec: ControllerSpec,
    ) -> Result<(TaskId, oneshot::Receiver<TaskOutcome>), ControllerError> {
        let id = TaskId::next();
        self.submit_prepared_and_watch(id, spec).await
    }

    pub(crate) async fn submit_prepared_and_watch(
        &self,
        id: TaskId,
        spec: ControllerSpec,
    ) -> Result<(TaskId, oneshot::Receiver<TaskOutcome>), ControllerError> {
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

    /// Tries to send a watched submission without waiting for command-channel capacity.
    ///
    /// The returned receiver has the same completion semantics as [`submit_and_watch`](Self::submit_and_watch).
    ///
    /// `ControllerError::Full` means the controller command channel is full, not that the target slot rejected the submission.
    #[cfg(test)]
    pub fn try_submit_and_watch(
        &self,
        spec: ControllerSpec,
    ) -> Result<(TaskId, oneshot::Receiver<TaskOutcome>), ControllerError> {
        let id = TaskId::next();
        self.try_submit_prepared_and_watch(id, spec)
    }

    pub(crate) fn try_submit_prepared_and_watch(
        &self,
        id: TaskId,
        spec: ControllerSpec,
    ) -> Result<(TaskId, oneshot::Receiver<TaskOutcome>), ControllerError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .try_send(ControllerCommand::Submit(Submission {
                id,
                spec,
                done: Some(tx),
            }))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => ControllerError::Full,
                mpsc::error::TrySendError::Closed(_) => ControllerError::Closed,
            })?;
        Ok((id, rx))
    }

    /// Sends one identity operation, waiting for command-channel capacity and then for the operation's reply.
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

    /// Enqueues one identity operation without waiting for command-channel capacity, then waits for its reply.
    ///
    /// Fail-fast behavior applies only to this first enqueue step.
    /// Registry fallback and terminal cleanup may still wait.
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

    pub(crate) async fn try_cancel(&self, id: TaskId) -> Result<bool, RuntimeError> {
        self.try_manage_identity(id, IdentityOperation::TryCancel)
            .await
    }

    pub(crate) async fn cancel_with_timeout(
        &self,
        id: TaskId,
        wait_for: std::time::Duration,
    ) -> Result<bool, RuntimeError> {
        self.manage_identity(id, IdentityOperation::CancelWithTimeout(wait_for))
            .await
    }

    pub(crate) async fn try_cancel_with_timeout(
        &self,
        id: TaskId,
        wait_for: std::time::Duration,
    ) -> Result<bool, RuntimeError> {
        self.try_manage_identity(id, IdentityOperation::TryCancelWithTimeout(wait_for))
            .await
    }
}
