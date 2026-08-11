//! Bounded registry-capacity admission and tracking for short-lived registry workers.

use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{Arc, Weak},
};

use futures_util::{future::BoxFuture, stream::FuturesUnordered};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::{
    RuntimeError,
    core::{AddReplyRx, ControllerAddPermit, RemovalCompletion, SupervisorCore},
    events::Event,
    identity::TaskId,
};

use super::{
    AdmissionResult, CompletionResult, Controller, RemovalResult,
    metadata::{MetadataResult, TaskNameSnapshot},
};

type CapacityFuture =
    Pin<Box<dyn Future<Output = Result<ControllerAddPermit, RuntimeError>> + Send + 'static>>;

/// Panic-contained futures owned and polled directly by the serialized controller loop.
pub(super) type WorkerSet<T> = FuturesUnordered<BoxFuture<'static, Result<T, String>>>;

/// All asynchronous controller-side work that does not need an independent Tokio task.
pub(super) struct ControllerWorkers {
    pub(super) metadata: WorkerSet<MetadataResult>,
    pub(super) capacity: CapacityAdmissionPump,
    pub(super) admissions: WorkerSet<AdmissionResult>,
    pub(super) completions: WorkerSet<CompletionResult>,
    pub(super) removals: WorkerSet<RemovalResult>,
    pub(super) identity_operations: WorkerSet<()>,
}

impl ControllerWorkers {
    pub(super) fn new(supervisor: Weak<SupervisorCore>, admission_capacity: usize) -> Self {
        Self {
            metadata: WorkerSet::new(),
            capacity: CapacityAdmissionPump::new(supervisor, admission_capacity),
            admissions: WorkerSet::new(),
            completions: WorkerSet::new(),
            removals: WorkerSet::new(),
            identity_operations: WorkerSet::new(),
        }
    }

    /// Tracks one isolated metadata callback without blocking later controller
    /// commands. Cancellation drops the receiver immediately; the fixed
    /// metadata worker retains the charged task until user code returns.
    pub(super) fn track_metadata(
        metadata: &WorkerSet<MetadataResult>,
        id: TaskId,
        cancel: CancellationToken,
        receiver: oneshot::Receiver<TaskNameSnapshot>,
    ) {
        Self::push(metadata, async move {
            let snapshot = tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                snapshot = receiver => snapshot.ok(),
            };
            MetadataResult { id, snapshot }
        });
    }

    pub(super) fn push<T>(set: &WorkerSet<T>, future: impl Future<Output = T> + Send + 'static)
    where
        T: 'static,
    {
        set.push(Box::pin(crate::core::panic_guard::guarded(future)));
    }
}

/// One result produced by the central registry-capacity pump.
pub(super) struct CapacityResult {
    pub(super) id: TaskId,
    pub(super) decision: Result<ControllerAddPermit, RuntimeError>,
}

/// Bounded FIFO that owns at most one `reserve_owned` future.
///
/// Keeping reservation futures as data avoids one Tokio task per blocked admission.
/// Removing the active identity drops its reservation future immediately, which removes the
/// corresponding waiter from Tokio's channel reservation queue.
pub(super) struct CapacityAdmissionPump {
    supervisor: Weak<SupervisorCore>,
    limit: usize,
    queued: VecDeque<TaskId>,
    active: Option<(TaskId, CapacityFuture)>,
}

impl CapacityAdmissionPump {
    pub(super) fn new(supervisor: Weak<SupervisorCore>, limit: usize) -> Self {
        debug_assert!(limit > 0);
        Self {
            supervisor,
            limit,
            queued: VecDeque::new(),
            active: None,
        }
    }

    pub(super) fn len(&self) -> usize {
        usize::from(self.active.is_some()) + self.queued.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.active.is_none() && self.queued.is_empty()
    }

    /// Enqueues one identity without exceeding the configured admission budget.
    pub(super) fn enqueue(&mut self, id: TaskId) -> Result<(), usize> {
        if self.len() >= self.limit {
            return Err(self.limit);
        }
        debug_assert!(
            self.active.as_ref().is_none_or(|(active, _)| *active != id)
                && !self.queued.contains(&id),
            "one TaskId cannot wait for registry capacity twice"
        );
        self.queued.push_back(id);
        self.start_next();
        Ok(())
    }

    /// Cancels a queued or active identity.
    ///
    /// Dropping an active reservation future is the cancellation acknowledgement; no ghost
    /// waiter remains in the registry command channel.
    pub(super) fn cancel(&mut self, id: TaskId) -> bool {
        if self
            .active
            .as_ref()
            .is_some_and(|(active, _)| *active == id)
        {
            self.active.take();
            self.start_next();
            return true;
        }
        let Some(position) = self.queued.iter().position(|queued| *queued == id) else {
            return false;
        };
        self.queued.remove(position);
        true
    }

    /// Waits for the active reservation and advances the FIFO.
    pub(super) async fn next(&mut self) -> Option<CapacityResult> {
        self.start_next();
        let (id, future) = self.active.as_mut()?;
        let id = *id;
        let decision = future.await;
        self.active.take();
        self.start_next();
        Some(CapacityResult { id, decision })
    }

    fn start_next(&mut self) {
        if self.active.is_some() {
            return;
        }
        let Some(id) = self.queued.pop_front() else {
            return;
        };
        let supervisor = self.supervisor.upgrade();
        let future: CapacityFuture = Box::pin(async move {
            match supervisor {
                Some(supervisor) => supervisor.reserve_controller_add().await,
                None => Err(RuntimeError::ShuttingDown),
            }
        });
        self.active = Some((id, future));
    }
}

impl Controller {
    /// Tracks a committed Add command until its direct registry reply arrives.
    pub(super) fn track_admission(
        admissions: &WorkerSet<AdmissionResult>,
        id: TaskId,
        slot_name: Arc<str>,
        reply: AddReplyRx,
        completion: RemovalCompletion,
    ) {
        ControllerWorkers::push(admissions, async move {
            let decision = match reply.await {
                Ok(Ok(())) => Ok(completion),
                Ok(Err(error)) => Err(error),
                Err(_) => Err(RuntimeError::ShuttingDown),
            };
            AdmissionResult {
                id,
                slot_name,
                decision,
            }
        });
    }

    /// Tracks one accepted task through logical registry cleanup and physical actor release.
    pub(super) fn track_completion(
        completions: &WorkerSet<CompletionResult>,
        id: TaskId,
        slot_name: Arc<str>,
        completion: RemovalCompletion,
    ) {
        ControllerWorkers::push(completions, async move {
            completion.wait_physical().await;
            CompletionResult { id, slot_name }
        });
    }

    /// Orders one runtime removal without blocking the controller loop on registry backpressure.
    pub(super) fn track_removal(
        removals: &WorkerSet<RemovalResult>,
        supervisor: Arc<SupervisorCore>,
        id: TaskId,
        slot_name: Arc<str>,
    ) {
        ControllerWorkers::push(removals, async move {
            let decision = supervisor.remove(id).await;
            RemovalResult {
                id,
                slot_name,
                decision,
            }
        });
    }

    /// Reports a failed removal request without changing slot ownership.
    ///
    /// Neither `Ok(true)` nor `Ok(false)` releases the slot.
    /// The reliable physical completion signal is the only normal release path.
    ///
    /// An error is diagnostic only; shutdown cleanup remains authoritative.
    pub(super) async fn handle_removal_result(&self, result: RemovalResult) {
        let Some(slot) = self.slot(&result.slot_name) else {
            return;
        };
        if slot.lock().await.owner_id() != Some(result.id) {
            return;
        }
        if let Err(error) = result.decision {
            self.bus.publish_lazy(|| {
                Event::runtime_failure(
                    "controller",
                    format!("remove_failed slot={}: {error}", result.slot_name),
                )
                .with_id(result.id)
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Weak;

    use crate::{RuntimeError, core::SupervisorCore, identity::TaskId};

    use super::CapacityAdmissionPump;

    #[test]
    fn capacity_pump_enforces_total_waiter_budget() {
        let mut pump = CapacityAdmissionPump::new(Weak::<SupervisorCore>::new(), 2);
        let first = TaskId::next();
        let second = TaskId::next();
        let rejected = TaskId::next();

        assert_eq!(pump.enqueue(first), Ok(()));
        assert_eq!(pump.enqueue(second), Ok(()));
        assert_eq!(pump.len(), 2);
        assert_eq!(pump.enqueue(rejected), Err(2));
        assert_eq!(pump.len(), 2);
    }

    #[test]
    fn cancelling_active_waiter_removes_it_and_advances_fifo() {
        let mut pump = CapacityAdmissionPump::new(Weak::<SupervisorCore>::new(), 2);
        let first = TaskId::next();
        let second = TaskId::next();
        pump.enqueue(first).unwrap();
        pump.enqueue(second).unwrap();

        assert!(pump.cancel(first));
        assert_eq!(pump.active.as_ref().map(|(id, _)| *id), Some(second));
        assert_eq!(pump.len(), 1);
        assert!(!pump.cancel(first));
        assert!(pump.cancel(second));
        assert!(pump.is_empty());
    }

    #[tokio::test]
    async fn dead_supervisor_resolves_waiter_without_leaking_queue_state() {
        let mut pump = CapacityAdmissionPump::new(Weak::<SupervisorCore>::new(), 1);
        let id = TaskId::next();
        pump.enqueue(id).unwrap();

        let result = pump.next().await.unwrap();
        assert_eq!(result.id, id);
        assert!(matches!(result.decision, Err(RuntimeError::ShuttingDown)));
        assert!(pump.is_empty());
    }
}
