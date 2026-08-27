//! Builds submission commands for the controller queue.
//!
//! Ordinary and prepared paths differ only in where the [`TaskId`] is allocated.
//! Watched paths attach an outcome sender. Waiting methods wait for command capacity,
//! while fail-fast methods reserve command capacity before taking ownership of the task.

use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::{
    TaskOutcome,
    controller::{ControllerError, ControllerSpec},
    identity::TaskId,
};

use super::{
    super::{ControllerCommand, Submission},
    ControllerHandle,
};

impl ControllerHandle {
    /// Allocates an identity and sends a submission through the waiting path.
    #[cfg(test)]
    pub(in crate::controller::engine) async fn submit(
        &self,
        spec: ControllerSpec,
    ) -> Result<TaskId, ControllerError> {
        let id = TaskId::next();
        self.submit_prepared(id, spec).await
    }

    /// Waits for ownership and command capacity, then sends a prepared submission.
    ///
    /// Success confirms command intake, not slot or registry admission.
    pub(crate) async fn submit_prepared(
        &self,
        id: TaskId,
        spec: ControllerSpec,
    ) -> Result<TaskId, ControllerError> {
        let owned = self.own(spec).await?;
        self.send_owned_prepared(id, owned, None).await
    }

    /// Bounds ownership admission, then waits normally for controller command capacity.
    pub(crate) async fn submit_prepared_with_ownership_timeout(
        &self,
        id: TaskId,
        spec: ControllerSpec,
        wait_for: Duration,
    ) -> Result<TaskId, ControllerError> {
        let owned = self.own_with_ownership_timeout(spec, wait_for).await?;
        self.send_owned_prepared(id, owned, None).await
    }

    /// Sends a task whose cleanup ownership has already been reserved.
    async fn send_owned_prepared(
        &self,
        id: TaskId,
        owned: crate::core::deferred_drop::OwnedTask<ControllerSpec>,
        done: Option<oneshot::Sender<TaskOutcome>>,
    ) -> Result<TaskId, ControllerError> {
        self.tx
            .send(ControllerCommand::Submit(Box::new(Submission {
                id,
                owned,
                done,
            })))
            .await
            .map_err(|_| ControllerError::Closed)?;
        Ok(id)
    }

    /// Allocates an identity and sends a submission through the fail-fast path.
    #[cfg(test)]
    pub(in crate::controller::engine) fn try_submit(
        &self,
        spec: ControllerSpec,
    ) -> Result<TaskId, ControllerError> {
        let id = TaskId::next();
        self.try_submit_prepared(id, spec)
    }

    /// Sends a prepared submission only when intake resources are available now.
    ///
    /// `ControllerError::Full` refers to command intake, not the target slot.
    pub(crate) fn try_submit_prepared(
        &self,
        id: TaskId,
        spec: ControllerSpec,
    ) -> Result<TaskId, ControllerError> {
        let permit = self.tx.try_reserve().map_err(|error| match error {
            mpsc::error::TrySendError::Full(()) => ControllerError::Full,
            mpsc::error::TrySendError::Closed(()) => ControllerError::Closed,
        })?;
        let owned = self.try_own(spec)?;
        permit.send(ControllerCommand::Submit(Box::new(Submission {
            id,
            owned,
            done: None,
        })));
        Ok(id)
    }

    /// Allocates an identity and sends a watched submission through the waiting path.
    #[cfg(test)]
    pub(in crate::controller::engine) async fn submit_and_watch(
        &self,
        spec: ControllerSpec,
    ) -> Result<(TaskId, oneshot::Receiver<TaskOutcome>), ControllerError> {
        let id = TaskId::next();
        self.submit_prepared_and_watch(id, spec).await
    }

    /// Waits for ownership and command capacity, then sends a watched submission.
    ///
    /// The receiver reports controller rejection or the runtime task outcome.
    pub(crate) async fn submit_prepared_and_watch(
        &self,
        id: TaskId,
        spec: ControllerSpec,
    ) -> Result<(TaskId, oneshot::Receiver<TaskOutcome>), ControllerError> {
        let owned = self.own(spec).await?;
        self.send_owned_prepared_and_watch(id, owned).await
    }

    /// Bounds ownership admission for a watched prepared submission.
    pub(crate) async fn submit_prepared_and_watch_with_ownership_timeout(
        &self,
        id: TaskId,
        spec: ControllerSpec,
        wait_for: Duration,
    ) -> Result<(TaskId, oneshot::Receiver<TaskOutcome>), ControllerError> {
        let owned = self.own_with_ownership_timeout(spec, wait_for).await?;
        self.send_owned_prepared_and_watch(id, owned).await
    }

    /// Creates the outcome channel only after cleanup ownership succeeds.
    async fn send_owned_prepared_and_watch(
        &self,
        id: TaskId,
        owned: crate::core::deferred_drop::OwnedTask<ControllerSpec>,
    ) -> Result<(TaskId, oneshot::Receiver<TaskOutcome>), ControllerError> {
        let (tx, rx) = oneshot::channel();
        self.send_owned_prepared(id, owned, Some(tx)).await?;
        Ok((id, rx))
    }

    /// Allocates an identity and sends a watched submission through the fail-fast path.
    #[cfg(test)]
    pub(in crate::controller::engine) fn try_submit_and_watch(
        &self,
        spec: ControllerSpec,
    ) -> Result<(TaskId, oneshot::Receiver<TaskOutcome>), ControllerError> {
        let id = TaskId::next();
        self.try_submit_prepared_and_watch(id, spec)
    }

    /// Sends a watched submission only when intake resources are available now.
    ///
    /// The receiver has the same result contract as [`Self::submit_prepared_and_watch`].
    pub(crate) fn try_submit_prepared_and_watch(
        &self,
        id: TaskId,
        spec: ControllerSpec,
    ) -> Result<(TaskId, oneshot::Receiver<TaskOutcome>), ControllerError> {
        let permit = self.tx.try_reserve().map_err(|error| match error {
            mpsc::error::TrySendError::Full(()) => ControllerError::Full,
            mpsc::error::TrySendError::Closed(()) => ControllerError::Closed,
        })?;
        let owned = self.try_own(spec)?;
        let (tx, rx) = oneshot::channel();
        permit.send(ControllerCommand::Submit(Box::new(Submission {
            id,
            owned,
            done: Some(tx),
        })));
        Ok((id, rx))
    }
}
