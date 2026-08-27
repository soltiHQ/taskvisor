//! Requests a task stop without waiting for terminal cleanup.
//!
//! [`SupervisorHandle`](crate::SupervisorHandle) uses this path directly.
//! The controller also uses it when an identity is no longer in its queued-work index.
//! Identity and name variants commit to the bounded registry queue before waiting for the registry's direct claim decision.
//! Name lookup and claim happen in the same registry command.
//!
//! `true` means this command claimed removal.
//! `false` means the target was absent or another stop path had already claimed it.
//! Either result can arrive before the actor reaches terminal cleanup.
//! Identity paths publish a best-effort `TaskRemoveRequested` event before queue commit.
//! Name paths publish it after the registry resolves the name to an identity.

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
    /// Identity removal with waiting queue admission.
    pub(crate) async fn remove(&self, id: TaskId) -> Result<bool, RuntimeError> {
        let reply = self.enqueue_remove_wait(id, None).await?;
        Self::await_remove_reply(reply).await
    }

    /// Identity removal with fail-fast queue admission.
    pub(crate) async fn try_remove(&self, id: TaskId) -> Result<bool, RuntimeError> {
        let reply = self.enqueue_remove(id, None)?;
        Self::await_remove_reply(reply).await
    }

    /// Atomic name removal with waiting queue admission.
    pub(in crate::core) async fn remove_by_name(
        &self,
        name: Arc<str>,
    ) -> Result<bool, RuntimeError> {
        let reply = self.enqueue_remove_by_name_wait(name).await?;
        Self::await_remove_reply(reply).await
    }

    /// Atomic name removal with fail-fast queue admission.
    pub(in crate::core) async fn try_remove_by_name(
        &self,
        name: Arc<str>,
    ) -> Result<bool, RuntimeError> {
        let reply = self.enqueue_remove_by_name(name)?;
        Self::await_remove_reply(reply).await
    }

    async fn await_remove_reply(reply: RemoveReplyRx) -> Result<bool, RuntimeError> {
        match reply.await {
            Ok(result) => result,
            Err(_) => Err(RuntimeError::ShuttingDown),
        }
    }

    /// Identity removal admitted immediately through the shutdown gate.
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

    /// Immediately commits one atomic name lookup and removal claim.
    fn enqueue_remove_by_name(&self, name: Arc<str>) -> Result<RemoveReplyRx, RuntimeError> {
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

        Ok(Self::commit_remove_by_name(permit, name))
    }

    async fn enqueue_remove_by_name_wait(
        &self,
        name: Arc<str>,
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

        Ok(Self::commit_remove_by_name(permit, name))
    }

    /// Sends a name command through an already-reserved queue slot.
    fn commit_remove_by_name(
        permit: mpsc::Permit<'_, RegistryCommand>,
        name: Arc<str>,
    ) -> RemoveReplyRx {
        let (reply, reply_rx) = oneshot::channel();
        permit.send(RegistryCommand::RemoveByName { name, reply });
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
