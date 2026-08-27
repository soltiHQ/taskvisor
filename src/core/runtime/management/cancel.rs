//! Cancels tasks and waits for shared registry cleanup.
//!
//! [`SupervisorHandle`](crate::SupervisorHandle) uses this path directly.
//! The controller uses it after an identity is absent from its queued-work index.
//! A command resolves an identity or name and claims removal at one registry ordering point.
//! Unlike `remove`, cancellation then waits for the registry's shared completion signal.
//!
//! The returned boolean records whether this call made the stop claim.
//! A caller that joins an existing claim waits for the same completion and returns `false`.
//! A missing task returns `false` without a completion wait.
//! Timeout variants start their timer after the registry decision.
//! A timeout ends only that caller's wait while removal continues.
//! Completion is logical registry cleanup.
//! A force-aborted attempt can remain physically active afterward.

use std::{sync::Arc, time::Duration};

use tokio::{
    sync::{mpsc, oneshot},
    time::timeout,
};

use super::super::SupervisorCore;
use crate::{
    core::registry::{CancelDecision, CancelReplyRx, RegistryCommand},
    error::RuntimeError,
    identity::TaskId,
};

impl SupervisorCore {
    /// Immediately admits an identity cancellation through the shutdown gate.
    fn enqueue_cancel(&self, id: TaskId) -> Result<CancelReplyRx, RuntimeError> {
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

        Ok(Self::commit_cancel(permit, id))
    }

    /// Waits for capacity and commits an identity cancellation through the shutdown gate.
    async fn enqueue_cancel_wait(&self, id: TaskId) -> Result<CancelReplyRx, RuntimeError> {
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

        Ok(Self::commit_cancel(permit, id))
    }

    /// Sends an identity cancellation through an already-reserved queue slot.
    fn commit_cancel(permit: mpsc::Permit<'_, RegistryCommand>, id: TaskId) -> CancelReplyRx {
        let (reply, reply_rx) = oneshot::channel();
        permit.send(RegistryCommand::Cancel { id, reply });
        reply_rx
    }

    /// Immediately commits one atomic name lookup and cancellation claim.
    fn enqueue_cancel_by_name(&self, name: Arc<str>) -> Result<CancelReplyRx, RuntimeError> {
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

        Ok(Self::commit_cancel_by_name(permit, name))
    }

    /// Waits for capacity before one atomic name lookup and cancellation claim.
    async fn enqueue_cancel_by_name_wait(
        &self,
        name: Arc<str>,
    ) -> Result<CancelReplyRx, RuntimeError> {
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

        Ok(Self::commit_cancel_by_name(permit, name))
    }

    /// Sends a name cancellation through an already-reserved queue slot.
    fn commit_cancel_by_name(
        permit: mpsc::Permit<'_, RegistryCommand>,
        name: Arc<str>,
    ) -> CancelReplyRx {
        let (reply, reply_rx) = oneshot::channel();
        permit.send(RegistryCommand::CancelByName { name, reply });
        reply_rx
    }

    /// Identity cancellation with unbounded logical completion wait.
    pub(crate) async fn cancel(&self, id: TaskId) -> Result<bool, RuntimeError> {
        let reply = self.enqueue_cancel_wait(id).await?;
        let decision = Self::await_cancel_reply(reply).await?;
        Self::wait_cancel_decision(decision, None).await
    }

    /// Identity cancellation with fail-fast queue admission and unbounded logical completion wait.
    pub(crate) async fn try_cancel(&self, id: TaskId) -> Result<bool, RuntimeError> {
        let decision = Self::await_cancel_reply(self.enqueue_cancel(id)?).await?;
        Self::wait_cancel_decision(decision, None).await
    }

    /// Identity cancellation with bounded logical completion wait.
    pub(crate) async fn cancel_with_timeout(
        &self,
        id: TaskId,
        wait_for: Duration,
    ) -> Result<bool, RuntimeError> {
        let reply = self.enqueue_cancel_wait(id).await?;
        let decision = Self::await_cancel_reply(reply).await?;
        Self::wait_cancel_decision(decision, Some(wait_for)).await
    }

    /// Identity cancellation with fail-fast queue admission and bounded logical completion wait.
    pub(crate) async fn try_cancel_with_timeout(
        &self,
        id: TaskId,
        wait_for: Duration,
    ) -> Result<bool, RuntimeError> {
        let decision = Self::await_cancel_reply(self.enqueue_cancel(id)?).await?;
        Self::wait_cancel_decision(decision, Some(wait_for)).await
    }

    /// Atomic name cancellation with unbounded logical completion wait.
    pub(in crate::core) async fn cancel_by_name(
        &self,
        name: Arc<str>,
    ) -> Result<bool, RuntimeError> {
        let reply = self.enqueue_cancel_by_name_wait(name).await?;
        let decision = Self::await_cancel_reply(reply).await?;
        Self::wait_cancel_decision(decision, None).await
    }

    /// Atomic name cancellation with fail-fast queue admission and unbounded logical completion wait.
    pub(in crate::core) async fn try_cancel_by_name(
        &self,
        name: Arc<str>,
    ) -> Result<bool, RuntimeError> {
        let decision = Self::await_cancel_reply(self.enqueue_cancel_by_name(name)?).await?;
        Self::wait_cancel_decision(decision, None).await
    }

    /// Atomic name cancellation with bounded logical completion wait.
    pub(in crate::core) async fn cancel_by_name_with_timeout(
        &self,
        name: Arc<str>,
        wait_for: Duration,
    ) -> Result<bool, RuntimeError> {
        let reply = self.enqueue_cancel_by_name_wait(name).await?;
        let decision = Self::await_cancel_reply(reply).await?;
        Self::wait_cancel_decision(decision, Some(wait_for)).await
    }

    /// Atomic name cancellation with fail-fast queue admission and bounded logical completion wait.
    pub(in crate::core) async fn try_cancel_by_name_with_timeout(
        &self,
        name: Arc<str>,
        wait_for: Duration,
    ) -> Result<bool, RuntimeError> {
        let decision = Self::await_cancel_reply(self.enqueue_cancel_by_name(name)?).await?;
        Self::wait_cancel_decision(decision, Some(wait_for)).await
    }

    /// Maps loss of the registry decision channel to shutdown.
    async fn await_cancel_reply(
        reply: CancelReplyRx,
    ) -> Result<Option<CancelDecision>, RuntimeError> {
        match reply.await {
            Ok(result) => result,
            Err(_) => Err(RuntimeError::ShuttingDown),
        }
    }

    /// Shared completion wait that preserves whether this caller owned the claim.
    ///
    /// A deadline is checked again against completion to avoid reporting a timeout when completion won at the same boundary.
    async fn wait_cancel_decision(
        decision: Option<CancelDecision>,
        wait_for: Option<Duration>,
    ) -> Result<bool, RuntimeError> {
        let Some(decision) = decision else {
            return Ok(false);
        };
        let id = decision.id;
        let claimed = decision.claimed;

        if let Some(wait_for) = wait_for {
            if timeout(wait_for, decision.wait()).await.is_err() && !decision.is_complete() {
                return Err(RuntimeError::TaskTerminationTimeout {
                    id,
                    timeout: wait_for,
                });
            }
        } else {
            decision.wait().await;
        }

        Ok(claimed)
    }
}
