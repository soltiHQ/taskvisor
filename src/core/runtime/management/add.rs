//! Moves direct tasks, controller handoffs, and static batches into registry admission.
//!
//! The operation returned by [`SupervisorHandle::add`](crate::SupervisorHandle::add) reserves
//! cleanup ownership and bounded queue capacity before it transfers a [`TaskSpec`].
//! The optional controller can reserve queue capacity while it retains its owned task,
//! then commit through the same final shutdown gate.
//! A non-empty static run sends its complete initial set as one `AddBatch` command.
//!
//! Direct add operations do not report success before the registry's reply.
//! Controller and static-run workflows receive the same direct reply channel.
//! Request and result events are observability only.
//! They do not confirm admission.

use std::{sync::Arc, time::Duration};

use tokio::sync::{mpsc, oneshot};

use super::super::SupervisorCore;
use crate::{
    core::{
        deferred_drop::OwnedTask,
        registry::{AddBatchItem, AddReplyRx, OutcomeTx, RegistryCommand, RemovalCompletion},
    },
    error::RuntimeError,
    events::{Event, EventKind},
    identity::TaskId,
    tasks::TaskSpec,
};

/// Keeps one registry queue slot while the controller retains its task payload.
#[cfg(feature = "controller")]
pub(crate) struct ControllerAddPermit {
    /// Owned capacity that can cross the controller's asynchronous admission step.
    permit: mpsc::OwnedPermit<RegistryCommand>,
}

impl SupervisorCore {
    /// Direct add after cleanup ownership, queue capacity, and the registry decision.
    pub(in crate::core) async fn add_task(&self, spec: TaskSpec) -> Result<TaskId, RuntimeError> {
        let (id, reply) = self
            .enqueue_add_task_wait(TaskId::next(), spec, None)
            .await
            .map_err(|(error, _done)| error)?;
        Self::await_add_reply(id, reply).await
    }

    /// Ownership-bounded direct add with the ordinary registry path.
    pub(in crate::core) async fn add_task_with_ownership_timeout(
        &self,
        spec: TaskSpec,
        wait_for: Duration,
    ) -> Result<TaskId, RuntimeError> {
        let (id, reply) = self
            .enqueue_add_task_wait_with_ownership_timeout(TaskId::next(), spec, None, wait_for)
            .await
            .map_err(|(error, _done)| error)?;
        Self::await_add_reply(id, reply).await
    }

    /// Fails fast on bounded capacity before waiting for the registry decision.
    pub(in crate::core) async fn try_add_task(
        &self,
        spec: TaskSpec,
    ) -> Result<TaskId, RuntimeError> {
        let (id, reply) = self
            .enqueue_add_task(TaskId::next(), spec, None)
            .await
            .map_err(|(error, _done)| error)?;
        Self::await_add_reply(id, reply).await
    }

    /// Watched task with fail-fast bounded-capacity admission.
    ///
    /// The returned receiver is available only after the registry accepts the task.
    pub(in crate::core) async fn try_add_task_watched(
        &self,
        spec: TaskSpec,
    ) -> Result<(TaskId, tokio::sync::oneshot::Receiver<crate::TaskOutcome>), RuntimeError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (id, reply) = self
            .enqueue_add_task(TaskId::next(), spec, Some(tx))
            .await
            .map_err(|(error, _done)| error)?;
        let id = Self::await_add_reply(id, reply).await?;
        Ok((id, rx))
    }

    /// Registry hand-off for a controller-owned task with an assigned identity.
    ///
    /// The returned completion tracks registry cleanup and full physical-attempt release.
    /// The controller waits for both before reusing the slot.
    /// Pre-commit failure returns the task and outcome sender inside [`crate::core::UncommittedWatchedAdd`].
    #[cfg(feature = "controller")]
    pub(crate) fn add_task_with_id_watched(
        &self,
        id: TaskId,
        name: Arc<str>,
        owned: OwnedTask<TaskSpec>,
        done: Option<OutcomeTx>,
    ) -> Result<(AddReplyRx, RemovalCompletion), Box<crate::core::UncommittedWatchedAdd>> {
        let completion = RemovalCompletion::new();
        let (_id, reply) = self.enqueue_named_add_task_with_completion_recovering(
            id,
            name,
            owned,
            done,
            Some(completion.clone()),
        )?;
        Ok((reply, completion))
    }

    /// Registry queue capacity reserved while the controller keeps the task value.
    ///
    /// Shutdown is checked again when the controller commits the permit.
    #[cfg(feature = "controller")]
    pub(crate) async fn reserve_controller_add(&self) -> Result<ControllerAddPermit, RuntimeError> {
        if self.is_shutting_down() {
            return Err(RuntimeError::ShuttingDown);
        }
        let permit = self
            .cmd_tx
            .clone()
            .reserve_owned()
            .await
            .map_err(|_| RuntimeError::ShuttingDown)?;
        Ok(ControllerAddPermit { permit })
    }

    /// Controller task transfer through a reserved slot after the final shutdown check.
    ///
    /// The returned completion covers registry cleanup and full physical-attempt release before slot reuse.
    /// A failed check returns every uncommitted user-owned value to the controller.
    #[cfg(feature = "controller")]
    pub(crate) fn commit_reserved_controller_add(
        &self,
        permit: ControllerAddPermit,
        id: TaskId,
        name: Arc<str>,
        owned: OwnedTask<TaskSpec>,
        done: Option<OutcomeTx>,
    ) -> Result<(AddReplyRx, RemovalCompletion), Box<crate::core::UncommittedWatchedAdd>> {
        let Some(_admission) = self.command_admission() else {
            return Err(Box::new(crate::core::UncommittedWatchedAdd {
                error: RuntimeError::ShuttingDown,
                name,
                owned,
                done,
            }));
        };

        let completion = RemovalCompletion::new();
        let (reply, reply_rx) = oneshot::channel();
        self.bus.publish_lazy(|| {
            Event::new(EventKind::TaskAddRequested)
                .with_task(Arc::clone(&name))
                .with_id(id)
        });
        permit.permit.send(RegistryCommand::Add {
            id,
            name,
            owned: Box::new(owned),
            outcome: done,
            completion: Some(completion.clone()),
            reply,
        });
        Ok((reply_rx, completion))
    }

    /// Watched task with waiting bounded-capacity admission.
    pub(in crate::core) async fn add_task_watched(
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

    /// Ownership-bounded watched add with the ordinary registry path.
    pub(in crate::core) async fn add_task_watched_with_ownership_timeout(
        &self,
        spec: TaskSpec,
        wait_for: Duration,
    ) -> Result<(TaskId, tokio::sync::oneshot::Receiver<crate::TaskOutcome>), RuntimeError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (id, reply) = self
            .enqueue_add_task_wait_with_ownership_timeout(TaskId::next(), spec, Some(tx), wait_for)
            .await
            .map_err(|(error, _done)| error)?;
        let id = Self::await_add_reply(id, reply).await?;
        Ok((id, rx))
    }

    async fn await_add_reply(id: TaskId, reply: AddReplyRx) -> Result<TaskId, RuntimeError> {
        match reply.await {
            Ok(Ok(())) => Ok(id),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(RuntimeError::ShuttingDown),
        }
    }

    /// Probes queue capacity before taking fail-fast cleanup ownership.
    ///
    /// It reserves queue capacity again under the final shutdown gate before commit.
    pub(in crate::core::runtime) async fn enqueue_add_task(
        &self,
        id: TaskId,
        spec: TaskSpec,
        mut done: Option<OutcomeTx>,
    ) -> Result<(TaskId, AddReplyRx), (RuntimeError, Option<OutcomeTx>)> {
        if self.is_shutting_down() {
            return Err((RuntimeError::ShuttingDown, done));
        }
        let initial_permit = self.cmd_tx.try_reserve().map_err(|error| {
            let error = match error {
                mpsc::error::TrySendError::Full(()) => RuntimeError::CommandQueueFull,
                mpsc::error::TrySendError::Closed(()) => RuntimeError::ShuttingDown,
            };
            (error, done.take())
        })?;
        drop(initial_permit);
        let reservation = self
            .drop_domain
            .try_reserve()
            .map_err(|error| (Self::ownership_admission_error(error), done.take()))?;
        let owned = self.own_task(spec, reservation);
        let name = owned.value.shared_name();
        let (permit, _admission) = match self.try_reserve_command_admission() {
            Ok(admission) => admission,
            Err(error) => return Err((error, done)),
        };
        Ok(self.commit_add(permit, id, name, owned, done, None))
    }

    #[cfg(feature = "controller")]
    fn enqueue_named_add_task_with_completion_recovering(
        &self,
        id: TaskId,
        name: Arc<str>,
        owned: OwnedTask<TaskSpec>,
        done: Option<OutcomeTx>,
        completion: Option<RemovalCompletion>,
    ) -> Result<(TaskId, AddReplyRx), Box<crate::core::UncommittedWatchedAdd>> {
        let (permit, _admission) = match self.try_reserve_command_admission() {
            Ok(admission) => admission,
            Err(error) => {
                return Err(Box::new(crate::core::UncommittedWatchedAdd {
                    error,
                    name,
                    owned,
                    done,
                }));
            }
        };
        Ok(self.commit_add(permit, id, name, owned, done, completion))
    }

    /// Cleanup ownership and queue capacity before the final shutdown gate.
    pub(in crate::core::runtime) async fn enqueue_add_task_wait(
        &self,
        id: TaskId,
        spec: TaskSpec,
        done: Option<OutcomeTx>,
    ) -> Result<(TaskId, AddReplyRx), (RuntimeError, Option<OutcomeTx>)> {
        self.enqueue_add_task_wait_inner(id, spec, done, None).await
    }

    /// Cleanup ownership bounded by `wait_for` before ordinary queue admission.
    pub(in crate::core::runtime) async fn enqueue_add_task_wait_with_ownership_timeout(
        &self,
        id: TaskId,
        spec: TaskSpec,
        done: Option<OutcomeTx>,
        wait_for: Duration,
    ) -> Result<(TaskId, AddReplyRx), (RuntimeError, Option<OutcomeTx>)> {
        self.enqueue_add_task_wait_inner(id, spec, done, Some(wait_for))
            .await
    }

    /// Shares the post-ownership queue and commit path between bounded and unbounded waits.
    async fn enqueue_add_task_wait_inner(
        &self,
        id: TaskId,
        spec: TaskSpec,
        mut done: Option<OutcomeTx>,
        ownership_timeout: Option<Duration>,
    ) -> Result<(TaskId, AddReplyRx), (RuntimeError, Option<OutcomeTx>)> {
        if self.is_shutting_down() {
            return Err((RuntimeError::ShuttingDown, done));
        }
        let reservation = match ownership_timeout {
            Some(wait_for) => {
                self.wait_for_ownership_with_timeout(self.reserve_ownership(), wait_for)
                    .await
            }
            None => self.wait_for_ownership(self.reserve_ownership()).await,
        }
        .map_err(|error| (error, done.take()))?;
        let owned = self.own_task(spec, reservation);
        let name = owned.value.shared_name();
        let permit = match tokio::select! {
            biased;
            _ = self.shutdown.started.cancelled() => Err(()),
            permit = self.cmd_tx.reserve() => permit.map_err(|_| ()),
        } {
            Ok(permit) => permit,
            Err(()) => return Err((RuntimeError::ShuttingDown, done)),
        };
        let Some(_admission) = self.command_admission() else {
            drop(permit);
            return Err((RuntimeError::ShuttingDown, done));
        };
        Ok(self.commit_add(permit, id, name, owned, done, None))
    }

    /// Request event followed immediately by an already-reserved add command.
    ///
    /// Pre-commit failures stay silent.
    /// The registry publishes its result later.
    fn commit_add(
        &self,
        permit: mpsc::Permit<'_, RegistryCommand>,
        id: TaskId,
        name: Arc<str>,
        owned: OwnedTask<TaskSpec>,
        done: Option<OutcomeTx>,
        completion: Option<RemovalCompletion>,
    ) -> (TaskId, AddReplyRx) {
        let (reply, reply_rx) = oneshot::channel();
        self.bus.publish_lazy(|| {
            Event::new(EventKind::TaskAddRequested)
                .with_task(Arc::clone(&name))
                .with_id(id)
        });
        permit.send(RegistryCommand::Add {
            id,
            name,
            owned: Box::new(owned),
            outcome: done,
            completion,
            reply,
        });
        (id, reply_rx)
    }

    /// Complete static batch committed through one queue slot and one gate check.
    pub(in crate::core::runtime) async fn enqueue_add_batch_wait(
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
            self.bus.publish_lazy(|| {
                Event::new(EventKind::TaskAddRequested)
                    .with_task(Arc::clone(&item.name))
                    .with_id(item.id)
            });
        }
        permit.send(RegistryCommand::AddBatch { items, reply });
        Ok(reply_rx)
    }

    /// Registry all-or-none decision for a static batch.
    pub(in crate::core::runtime) async fn await_add_batch_reply(
        reply: AddReplyRx,
    ) -> Result<(), RuntimeError> {
        match reply.await {
            Ok(result) => result,
            Err(_) => Err(RuntimeError::ShuttingDown),
        }
    }
}
