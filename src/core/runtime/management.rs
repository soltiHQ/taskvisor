//! Registry management gateway and shutdown admission fence.
//!
//! Queue capacity is reserved before the final admission check.
//! The same mutex covers that re-check and command commit, while shutdown closes the gate before awaiting the registry fence.

use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use tokio::{
    sync::{mpsc, oneshot},
    time::timeout,
};

use super::SupervisorCore;
use crate::{
    core::registry::{
        AddBatchItem, AddReplyRx, CancelDecision, CancelReplyRx, OutcomeTx, RegistryCommand,
        RemovalCompletion, RemoveReplyRx,
    },
    error::RuntimeError,
    events::{Event, EventKind},
    identity::TaskId,
    tasks::TaskSpec,
};

impl SupervisorCore {
    /// Marks the runtime as shutting down.
    ///
    /// The gate lock waits for every command that already passed its final admission check to become visible in the registry queue.
    pub(super) fn mark_shutting_down(&self) {
        let _gate = self
            .admission_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.shutting_down.store(true, Ordering::Release);
    }

    /// Holds the admission gate across a command's final check and queue commit.
    fn command_admission(&self) -> Option<std::sync::MutexGuard<'_, ()>> {
        let gate = self
            .admission_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.is_shutting_down() {
            None
        } else {
            Some(gate)
        }
    }

    /// Closes command admission and waits until the registry reaches that ordering point.
    ///
    /// Every command committed before the gate closes is ahead of this fence.
    /// Backpressured callers re-check the gate after receiving capacity and are rejected instead of appearing behind the fence.
    pub(super) async fn close_admission_and_fence_registry(&self) -> Result<(), RuntimeError> {
        self.mark_shutting_down();
        self.registry.fence().await
    }

    /// Adds a task and waits for the registry registration decision.
    ///
    /// This waits for queue capacity before sending the command.
    pub(crate) async fn add_task(&self, spec: TaskSpec) -> Result<TaskId, RuntimeError> {
        let (id, reply) = self
            .enqueue_add_task_wait(TaskId::next(), spec, None)
            .await
            .map_err(|(error, _done)| error)?;
        Self::await_add_reply(id, reply).await
    }

    /// Tries to add a task without waiting for queue capacity.
    ///
    /// After the command enters the queue, this still waits for the registry registration decision.
    pub(crate) async fn try_add_task(&self, spec: TaskSpec) -> Result<TaskId, RuntimeError> {
        let (id, reply) = self
            .enqueue_add_task(TaskId::next(), spec, None)
            .map_err(|(error, _done)| error)?;
        Self::await_add_reply(id, reply).await
    }

    /// Tries to add a watched task without waiting for queue capacity.
    ///
    /// After the command enters the queue, this still waits for the registry registration decision.
    pub(crate) async fn try_add_task_watched(
        &self,
        spec: TaskSpec,
    ) -> Result<(TaskId, tokio::sync::oneshot::Receiver<crate::TaskOutcome>), RuntimeError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (id, reply) = self
            .enqueue_add_task(TaskId::next(), spec, Some(tx))
            .map_err(|(error, _done)| error)?;
        let id = Self::await_add_reply(id, reply).await?;
        Ok((id, rx))
    }

    /// Queues a task add command under a pre-minted identity.
    ///
    /// Used by the controller so a submission keeps the same [`TaskId`] from admission through registry registration.
    /// Returns the direct Add reply and a shared signal that resolves only after terminal registry cleanup releases the task id and label.
    #[cfg(feature = "controller")]
    pub(crate) fn add_task_with_id_watched(
        &self,
        id: TaskId,
        spec: TaskSpec,
        done: Option<OutcomeTx>,
    ) -> Result<(AddReplyRx, RemovalCompletion), (RuntimeError, Option<OutcomeTx>)> {
        let completion = RemovalCompletion::new();
        let (_id, reply) =
            self.enqueue_add_task_with_completion(id, spec, done, Some(completion.clone()))?;
        Ok((reply, completion))
    }

    /// Adds a watched task and waits for the registry registration decision.
    ///
    /// Returns the minted [`TaskId`] and a receiver that resolves to the final [`TaskOutcome`](crate::TaskOutcome) if the task is registered and later terminates.
    pub(crate) async fn add_task_watched(
        &self,
        spec: TaskSpec,
    ) -> Result<(TaskId, tokio::sync::oneshot::Receiver<crate::TaskOutcome>), RuntimeError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (id, reply) = self
            .enqueue_add_task_wait(TaskId::next(), spec, Some(tx))
            .await
            .map_err(|(error, _done)| error)?;
        let id = Self::await_add_reply(id, reply).await?;
        Ok((id, rx))
    }

    /// Resolves one authoritative registry Add reply.
    async fn await_add_reply(id: TaskId, reply: AddReplyRx) -> Result<TaskId, RuntimeError> {
        match reply.await {
            Ok(Ok(())) => Ok(id),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(RuntimeError::ShuttingDown),
        }
    }

    /// Queues one add command and returns its authoritative registry reply.
    ///
    /// Used by `try_add` and the controller's fail-fast admission path.
    pub(super) fn enqueue_add_task(
        &self,
        id: TaskId,
        spec: TaskSpec,
        done: Option<OutcomeTx>,
    ) -> Result<(TaskId, AddReplyRx), (RuntimeError, Option<OutcomeTx>)> {
        self.enqueue_add_task_with_completion(id, spec, done, None)
    }

    /// Queues one Add command with an optional shared terminal completion signal.
    fn enqueue_add_task_with_completion(
        &self,
        id: TaskId,
        spec: TaskSpec,
        done: Option<OutcomeTx>,
        completion: Option<RemovalCompletion>,
    ) -> Result<(TaskId, AddReplyRx), (RuntimeError, Option<OutcomeTx>)> {
        if self.is_shutting_down() {
            return Err((RuntimeError::ShuttingDown, done));
        }
        let label: Arc<str> = Arc::from(spec.task().name());
        let permit = match self.cmd_tx.try_reserve() {
            Ok(permit) => permit,
            Err(mpsc::error::TrySendError::Full(())) => {
                return Err((RuntimeError::CommandQueueFull, done));
            }
            Err(mpsc::error::TrySendError::Closed(())) => {
                return Err((RuntimeError::ShuttingDown, done));
            }
        };
        let Some(_admission) = self.command_admission() else {
            drop(permit);
            return Err((RuntimeError::ShuttingDown, done));
        };
        Ok(self.commit_add(permit, id, label, spec, done, completion))
    }

    /// Waits for bounded queue capacity, then queues one Add command.
    pub(super) async fn enqueue_add_task_wait(
        &self,
        id: TaskId,
        spec: TaskSpec,
        done: Option<OutcomeTx>,
    ) -> Result<(TaskId, AddReplyRx), (RuntimeError, Option<OutcomeTx>)> {
        if self.is_shutting_down() {
            return Err((RuntimeError::ShuttingDown, done));
        }
        let label: Arc<str> = Arc::from(spec.task().name());
        let permit = match self.cmd_tx.reserve().await {
            Ok(permit) => permit,
            Err(_) => return Err((RuntimeError::ShuttingDown, done)),
        };
        let Some(_admission) = self.command_admission() else {
            drop(permit);
            return Err((RuntimeError::ShuttingDown, done));
        };
        Ok(self.commit_add(permit, id, label, spec, done, None))
    }

    /// Publishes the request event and makes an already-reserved Add visible.
    ///
    /// Reserving capacity before this call keeps rejected commands silent while preserving `TaskAddRequested` before the registry result event.
    fn commit_add(
        &self,
        permit: mpsc::Permit<'_, RegistryCommand>,
        id: TaskId,
        label: Arc<str>,
        spec: TaskSpec,
        done: Option<OutcomeTx>,
        completion: Option<RemovalCompletion>,
    ) -> (TaskId, AddReplyRx) {
        let (reply, reply_rx) = oneshot::channel();
        self.bus.publish(
            Event::new(EventKind::TaskAddRequested)
                .with_task(label)
                .with_id(id),
        );
        permit.send(RegistryCommand::Add {
            id,
            spec,
            outcome: done,
            completion,
            reply,
        });
        (id, reply_rx)
    }

    /// Waits for one queue slot, then commits the complete static task batch.
    pub(super) async fn enqueue_add_batch_wait(
        &self,
        items: Vec<AddBatchItem>,
    ) -> Result<AddReplyRx, RuntimeError> {
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

        let (reply, reply_rx) = oneshot::channel();
        for item in &items {
            self.bus.publish(
                Event::new(EventKind::TaskAddRequested)
                    .with_task(Arc::clone(&item.label))
                    .with_id(item.id),
            );
        }
        permit.send(RegistryCommand::AddBatch { items, reply });
        Ok(reply_rx)
    }

    /// Resolves the authoritative decision for one static registration batch.
    pub(super) async fn await_add_batch_reply(reply: AddReplyRx) -> Result<(), RuntimeError> {
        match reply.await {
            Ok(result) => result,
            Err(_) => Err(RuntimeError::ShuttingDown),
        }
    }

    /// Requests removal by identity and returns whether this command claimed the registered task.
    ///
    /// This does not wait for terminal cleanup.
    pub(crate) async fn remove(&self, id: TaskId) -> Result<bool, RuntimeError> {
        let reply = self.enqueue_remove_wait(id, None).await?;
        Self::await_remove_reply(reply).await
    }

    /// Tries to request removal without waiting for command queue capacity.
    ///
    /// The result is the registry claim decision, not terminal cleanup.
    pub(crate) async fn try_remove(&self, id: TaskId) -> Result<bool, RuntimeError> {
        let reply = self.enqueue_remove(id, None)?;
        Self::await_remove_reply(reply).await
    }

    /// Requests removal of the task that owns `label` at the registry ordering point.
    ///
    /// This does not wait for terminal cleanup.
    pub(crate) async fn remove_by_label(&self, label: Arc<str>) -> Result<bool, RuntimeError> {
        let reply = self.enqueue_remove_by_label_wait(label).await?;
        Self::await_remove_reply(reply).await
    }

    /// Tries to request label-based removal without waiting for command queue capacity.
    ///
    /// The result is the registry claim decision, not terminal cleanup.
    pub(crate) async fn try_remove_by_label(&self, label: Arc<str>) -> Result<bool, RuntimeError> {
        let reply = self.enqueue_remove_by_label(label)?;
        Self::await_remove_reply(reply).await
    }

    /// Resolves one authoritative registry Remove reply.
    async fn await_remove_reply(reply: RemoveReplyRx) -> Result<bool, RuntimeError> {
        match reply.await {
            Ok(result) => result,
            Err(_) => Err(RuntimeError::ShuttingDown),
        }
    }

    /// Publishes one remove request, queues its command, and returns the authoritative reply.
    pub(super) fn enqueue_remove(
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

    /// Waits for bounded queue capacity, then queues one Remove command.
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

    /// Queues one fail-fast atomic label Remove command.
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

    /// Waits for queue capacity, then sends one atomic label Remove command.
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

    /// Makes one already-reserved Remove command by label visible to the registry.
    fn commit_remove_by_label(
        permit: mpsc::Permit<'_, RegistryCommand>,
        label: Arc<str>,
    ) -> RemoveReplyRx {
        let (reply, reply_rx) = oneshot::channel();
        permit.send(RegistryCommand::RemoveByLabel { label, reply });
        reply_rx
    }

    /// Publishes one identity request and makes its reserved command visible.
    fn commit_remove(
        &self,
        permit: mpsc::Permit<'_, RegistryCommand>,
        id: TaskId,
        reason: Option<&'static str>,
    ) -> RemoveReplyRx {
        let (reply, reply_rx) = oneshot::channel();
        let mut event = Event::new(EventKind::TaskRemoveRequested).with_id(id);
        if let Some(reason) = reason {
            event = event.with_reason(reason);
        }
        self.bus.publish(event);
        permit.send(RegistryCommand::Remove { id, reply });
        reply_rx
    }

    /// Queues one fail-fast Cancel command by identity.
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

    /// Waits for bounded queue capacity, then queues one Cancel command by identity.
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

    /// Makes one already-reserved Cancel command by identity visible to the registry.
    fn commit_cancel(permit: mpsc::Permit<'_, RegistryCommand>, id: TaskId) -> CancelReplyRx {
        let (reply, reply_rx) = oneshot::channel();
        permit.send(RegistryCommand::Cancel { id, reply });
        reply_rx
    }

    /// Queues one fail-fast atomic Cancel command by label.
    fn enqueue_cancel_by_label(&self, label: Arc<str>) -> Result<CancelReplyRx, RuntimeError> {
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

        Ok(Self::commit_cancel_by_label(permit, label))
    }

    /// Waits for bounded queue capacity, then queues one atomic Cancel command by label.
    async fn enqueue_cancel_by_label_wait(
        &self,
        label: Arc<str>,
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

        Ok(Self::commit_cancel_by_label(permit, label))
    }

    /// Makes one already-reserved Cancel command by label visible to the registry.
    fn commit_cancel_by_label(
        permit: mpsc::Permit<'_, RegistryCommand>,
        label: Arc<str>,
    ) -> CancelReplyRx {
        let (reply, reply_rx) = oneshot::channel();
        permit.send(RegistryCommand::CancelByLabel { label, reply });
        reply_rx
    }

    /// Returns registered tasks as `(id, label)` pairs from the registry.
    pub(crate) async fn list_tasks(&self) -> Vec<(TaskId, Arc<str>)> {
        self.registry.list().await
    }

    /// Returns true if `id` is currently registered.
    #[cfg(test)]
    pub(crate) async fn contains_id(&self, id: TaskId) -> bool {
        self.registry.contains(id).await
    }

    /// Returns currently available registry command slots for controller backpressure tests.
    #[cfg(all(test, feature = "controller"))]
    pub(crate) fn registry_command_capacity(&self) -> usize {
        self.cmd_tx.capacity()
    }

    /// Resolves a label to the identity currently holding it (if any).
    #[cfg(test)]
    pub(crate) async fn id_for_label(&self, name: &str) -> Option<TaskId> {
        self.registry.id_for_label(name).await
    }

    /// Cancels a task by identity and waits for registry terminal completion.
    pub(crate) async fn cancel(&self, id: TaskId) -> Result<bool, RuntimeError> {
        let reply = self.enqueue_cancel_wait(id).await?;
        let decision = Self::await_cancel_reply(reply).await?;
        Self::wait_cancel_decision(decision, None).await
    }

    /// Tries to cancel a task by identity without waiting for command queue capacity.
    ///
    /// After the command enters the queue, this still waits for the registry decision and terminal completion.
    pub(crate) async fn try_cancel(&self, id: TaskId) -> Result<bool, RuntimeError> {
        let decision = Self::await_cancel_reply(self.enqueue_cancel(id)?).await?;
        Self::wait_cancel_decision(decision, None).await
    }

    /// Cancels a task with an explicit confirmation window.
    ///
    /// The registry decision is not part of `wait_for`.
    /// The timeout only bounds this caller's wait for shared terminal completion and does not stop removal.
    pub(crate) async fn cancel_with_timeout(
        &self,
        id: TaskId,
        wait_for: Duration,
    ) -> Result<bool, RuntimeError> {
        let reply = self.enqueue_cancel_wait(id).await?;
        let decision = Self::await_cancel_reply(reply).await?;
        Self::wait_cancel_decision(decision, Some(wait_for)).await
    }

    /// Tries to cancel a task without waiting for command queue capacity and bounds the terminal completion wait after registry admission.
    pub(crate) async fn try_cancel_with_timeout(
        &self,
        id: TaskId,
        wait_for: Duration,
    ) -> Result<bool, RuntimeError> {
        let decision = Self::await_cancel_reply(self.enqueue_cancel(id)?).await?;
        Self::wait_cancel_decision(decision, Some(wait_for)).await
    }

    /// Cancels the task that owns `label` at the registry ordering point.
    pub(crate) async fn cancel_by_label(&self, label: Arc<str>) -> Result<bool, RuntimeError> {
        let reply = self.enqueue_cancel_by_label_wait(label).await?;
        let decision = Self::await_cancel_reply(reply).await?;
        Self::wait_cancel_decision(decision, None).await
    }

    /// Tries to cancel the task that owns `label` without waiting for command queue capacity.
    pub(crate) async fn try_cancel_by_label(&self, label: Arc<str>) -> Result<bool, RuntimeError> {
        let decision = Self::await_cancel_reply(self.enqueue_cancel_by_label(label)?).await?;
        Self::wait_cancel_decision(decision, None).await
    }

    /// Cancels the task that owns `label` with an explicit terminal-completion window.
    ///
    /// Queue admission and the registry decision are not part of `wait_for`.
    /// The timeout only bounds this caller's wait for shared terminal completion and does not stop removal.
    pub(crate) async fn cancel_by_label_with_timeout(
        &self,
        label: Arc<str>,
        wait_for: Duration,
    ) -> Result<bool, RuntimeError> {
        let reply = self.enqueue_cancel_by_label_wait(label).await?;
        let decision = Self::await_cancel_reply(reply).await?;
        Self::wait_cancel_decision(decision, Some(wait_for)).await
    }

    /// Tries to cancel the task that owns `label` without waiting for command queue capacity and bounds the terminal-completion wait after the registry decision.
    pub(crate) async fn try_cancel_by_label_with_timeout(
        &self,
        label: Arc<str>,
        wait_for: Duration,
    ) -> Result<bool, RuntimeError> {
        let decision = Self::await_cancel_reply(self.enqueue_cancel_by_label(label)?).await?;
        Self::wait_cancel_decision(decision, Some(wait_for)).await
    }

    /// Resolves one authoritative registry cancellation decision.
    async fn await_cancel_reply(
        reply: CancelReplyRx,
    ) -> Result<Option<CancelDecision>, RuntimeError> {
        match reply.await {
            Ok(result) => result,
            Err(_) => Err(RuntimeError::ShuttingDown),
        }
    }

    /// Waits for one shared terminal completion and preserves its claim result.
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
