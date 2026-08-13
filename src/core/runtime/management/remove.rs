//! Requests a task stop without waiting for terminal cleanup.
//!
//! [`SupervisorHandle`](crate::SupervisorHandle) uses this path directly.
//! The controller also uses it when an identity is no longer in its queued-work index.
//! Identity and label variants commit to the bounded registry queue, then wait
//! for the registry's direct claim decision.
//! Label lookup and claim happen in the same registry command.
//!
//! `true` means this command claimed removal. `false` means the target was absent
//! or another stop path had already claimed it. Either result can arrive before
//! the actor reaches terminal cleanup.
//! Identity paths publish a best-effort `TaskRemoveRequested` event before queue commit.
//! Label paths publish it after the registry resolves the label to an identity.

use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use super::super::SupervisorCore;
use crate::{
    core::registry::{RegistryCommand, RemoveReplyRx},
    error::RuntimeError,
    events::{Event, EventKind},
    identity::TaskId,
};

impl SupervisorCore {
    /// Waits for queue capacity and returns the identity claim decision.
    pub(crate) async fn remove(&self, id: TaskId) -> Result<bool, RuntimeError> {
        let reply = self.enqueue_remove_wait(id, None).await?;
        Self::await_remove_reply(reply).await
    }

    /// Uses immediate queue admission before waiting for the identity claim decision.
    pub(crate) async fn try_remove(&self, id: TaskId) -> Result<bool, RuntimeError> {
        let reply = self.enqueue_remove(id, None)?;
        Self::await_remove_reply(reply).await
    }

    /// Waits for queue capacity and atomically claims the current owner of `label`.
    pub(in crate::core) async fn remove_by_label(
        &self,
        label: Arc<str>,
    ) -> Result<bool, RuntimeError> {
        let reply = self.enqueue_remove_by_label_wait(label).await?;
        Self::await_remove_reply(reply).await
    }

    /// Uses immediate queue admission for an atomic label lookup and claim.
    pub(in crate::core) async fn try_remove_by_label(
        &self,
        label: Arc<str>,
    ) -> Result<bool, RuntimeError> {
        let reply = self.enqueue_remove_by_label(label)?;
        Self::await_remove_reply(reply).await
    }

    /// Maps loss of the registry reply channel to shutdown.
    async fn await_remove_reply(reply: RemoveReplyRx) -> Result<bool, RuntimeError> {
        match reply.await {
            Ok(result) => result,
            Err(_) => Err(RuntimeError::ShuttingDown),
        }
    }

    /// Immediately admits an identity removal through the shutdown gate.
    pub(in crate::core::runtime) fn enqueue_remove(
        &self,
        id: TaskId,
        reason: Option<&'static str>,
    ) -> Result<RemoveReplyRx, RuntimeError> {
        if self.is_shutting_down() {
            return Err(RuntimeError::ShuttingDown);
        }
        let permit = self.cmd_tx.try_reserve().map_err(|error| match error {
            mpsc::error::TrySendError::Full(()) => RuntimeError::CommandQueueFull,
            mpsc::error::TrySendError::Closed(()) => RuntimeError::ShuttingDown,
        })?;
        let Some(_admission) = self.command_admission() else {
            drop(permit);
            return Err(RuntimeError::ShuttingDown);
        };
        Ok(self.commit_remove(permit, id, reason))
    }

    /// Waits for capacity and commits an identity removal through the shutdown gate.
    async fn enqueue_remove_wait(
        &self,
        id: TaskId,
        reason: Option<&'static str>,
    ) -> Result<RemoveReplyRx, RuntimeError> {
        if self.is_shutting_down() {
            return Err(RuntimeError::ShuttingDown);
        }
        let permit = self
            .cmd_tx
            .reserve()
            .await
            .map_err(|_| RuntimeError::ShuttingDown)?;
        let Some(_admission) = self.command_admission() else {
            drop(permit);
            return Err(RuntimeError::ShuttingDown);
        };
        Ok(self.commit_remove(permit, id, reason))
    }

    /// Immediately commits one atomic label lookup and removal claim.
    fn enqueue_remove_by_label(&self, label: Arc<str>) -> Result<RemoveReplyRx, RuntimeError> {
        if self.is_shutting_down() {
            return Err(RuntimeError::ShuttingDown);
        }
        let permit = self.cmd_tx.try_reserve().map_err(|error| match error {
            mpsc::error::TrySendError::Full(()) => RuntimeError::CommandQueueFull,
            mpsc::error::TrySendError::Closed(()) => RuntimeError::ShuttingDown,
        })?;
        let Some(_admission) = self.command_admission() else {
            drop(permit);
            return Err(RuntimeError::ShuttingDown);
        };

        Ok(Self::commit_remove_by_label(permit, label))
    }

    /// Waits for capacity before one atomic label lookup and removal claim.
    async fn enqueue_remove_by_label_wait(
        &self,
        label: Arc<str>,
    ) -> Result<RemoveReplyRx, RuntimeError> {
        if self.is_shutting_down() {
            return Err(RuntimeError::ShuttingDown);
        }
        let permit = self
            .cmd_tx
            .reserve()
            .await
            .map_err(|_| RuntimeError::ShuttingDown)?;
        let Some(_admission) = self.command_admission() else {
            drop(permit);
            return Err(RuntimeError::ShuttingDown);
        };

        Ok(Self::commit_remove_by_label(permit, label))
    }

    /// Sends a label command through an already-reserved queue slot.
    fn commit_remove_by_label(
        permit: mpsc::Permit<'_, RegistryCommand>,
        label: Arc<str>,
    ) -> RemoveReplyRx {
        let (reply, reply_rx) = oneshot::channel();
        permit.send(RegistryCommand::RemoveByLabel { label, reply });
        reply_rx
    }

    /// Publishes the identity request event before sending its reserved command.
    fn commit_remove(
        &self,
        permit: mpsc::Permit<'_, RegistryCommand>,
        id: TaskId,
        reason: Option<&'static str>,
    ) -> RemoveReplyRx {
        let (reply, reply_rx) = oneshot::channel();
        self.bus.publish_lazy(|| {
            let mut event = Event::new(EventKind::TaskRemoveRequested).with_id(id);
            if let Some(reason) = reason {
                event = event.with_reason(reason);
            }
            event
        });
        permit.send(RegistryCommand::Remove { id, reply });
        reply_rx
    }
}
