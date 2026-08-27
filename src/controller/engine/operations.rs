//! In-loop operations tracked by the controller driver.
//!
//! The lifecycle driver owns registry-capacity waits, admission replies, physical completion, removal, and identity work.
//! This keeps state transitions in one loop without spawning a Tokio task per operation.

use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{Arc, Weak},
};

use futures_util::{future::BoxFuture, stream::FuturesUnordered};

use crate::{
    RuntimeError,
    core::{AddReplyRx, ControllerAddPermit, RemovalCompletion, SupervisorCore},
    events::Event,
    identity::TaskId,
};

use super::{AdmissionResult, CompletionResult, Controller, RemovalResult};

type CapacityFuture =
    Pin<Box<dyn Future<Output = Result<ControllerAddPermit, RuntimeError>> + Send + 'static>>;

/// Panic-contained futures owned and polled directly by the serialized controller loop.
pub(super) type OperationSet<T> = FuturesUnordered<BoxFuture<'static, Result<T, String>>>;

/// All asynchronous controller-side work that does not need an independent Tokio task.
pub(super) struct TrackedOperations {
    /// Ordered submissions waiting for registry command capacity.
    pub(super) capacity: CapacityAdmissionPump,
    /// Registry admission replies.
    pub(super) admissions: OperationSet<AdmissionResult>,
    /// Physical runtime completion signals.
    pub(super) completions: OperationSet<CompletionResult>,
    /// Results of requested owner removal.
    pub(super) removals: OperationSet<RemovalResult>,
    /// Registry fallback for identity commands.
    pub(super) identity_operations: OperationSet<()>,
}

impl TrackedOperations {
    /// Creates empty tracked operation sets and a bounded capacity queue.
    pub(super) fn new(supervisor: Weak<SupervisorCore>, admission_capacity: usize) -> Self {
        Self {
            capacity: CapacityAdmissionPump::new(supervisor, admission_capacity),
            admissions: OperationSet::new(),
            completions: OperationSet::new(),
            removals: OperationSet::new(),
            identity_operations: OperationSet::new(),
        }
    }

    /// Panic-contained registration for one in-loop future.
    pub(super) fn push<T>(set: &OperationSet<T>, future: impl Future<Output = T> + Send + 'static)
    where
        T: 'static,
    {
        set.push(Box::pin(crate::core::panic_guard::guarded(future)));
    }
}

/// One result produced by the central registry-capacity pump.
pub(super) struct CapacityResult {
    /// Task identity whose reservation completed.
    pub(super) id: TaskId,
    /// Registry command permit or reservation failure.
    pub(super) decision: Result<ControllerAddPermit, RuntimeError>,
}

/// FIFO queue for bounded registry-capacity reservations.
///
/// At most one reservation future is active.
/// Canceling that identity drops its future and removes its waiter from the registry command channel.
pub(super) struct CapacityAdmissionPump {
    /// Runtime registry used to reserve command capacity.
    supervisor: Weak<SupervisorCore>,
    /// Maximum active and queued reservations.
    limit: usize,
    /// Waiting task identities in queue order.
    queued: VecDeque<TaskId>,
    /// Single reservation future currently being polled.
    active: Option<(TaskId, CapacityFuture)>,
}

impl CapacityAdmissionPump {
    /// Creates an empty bounded reservation queue.
    pub(super) fn new(supervisor: Weak<SupervisorCore>, limit: usize) -> Self {
        debug_assert!(limit > 0);
        Self {
            supervisor,
            limit,
            queued: VecDeque::new(),
            active: None,
        }
    }

    /// Returns active and queued reservation count.
    pub(super) fn len(&self) -> usize {
        usize::from(self.active.is_some()) + self.queued.len()
    }

    /// Returns whether no reservation is active or queued.
    pub(super) fn is_empty(&self) -> bool {
        self.active.is_none() && self.queued.is_empty()
    }

    /// Bounded FIFO admission for one registry-capacity waiter.
    ///
    /// A full pump leaves its state unchanged and reports the configured limit.
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

    /// Cancellation of one queued or active capacity waiter.
    ///
    /// Dropping an active reservation future acknowledges cancellation.
    /// No ghost waiter remains in the registry command channel.
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

    /// Waits for the active reservation and advances the queue.
    pub(super) async fn next(&mut self) -> Option<CapacityResult> {
        self.start_next();
        let (id, future) = self.active.as_mut()?;
        let id = *id;
        let decision = future.await;
        self.active.take();
        self.start_next();
        Some(CapacityResult { id, decision })
    }

    /// Starts the next queued reservation when none is active.
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
    /// Direct registry-decision tracking for one committed Add command.
    pub(super) fn track_admission(
        admissions: &OperationSet<AdmissionResult>,
        id: TaskId,
        slot_name: Arc<str>,
        reply: AddReplyRx,
        completion: RemovalCompletion,
    ) {
        TrackedOperations::push(admissions, async move {
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

    /// Physical-completion tracking for one accepted task.
    pub(super) fn track_completion(
        completions: &OperationSet<CompletionResult>,
        id: TaskId,
        slot_name: Arc<str>,
        completion: RemovalCompletion,
    ) {
        TrackedOperations::push(completions, async move {
            completion.wait_physical().await;
            CompletionResult { id, slot_name }
        });
    }

    /// Non-blocking runtime-removal tracking for one slot owner.
    pub(super) fn track_removal(
        removals: &OperationSet<RemovalResult>,
        supervisor: Arc<SupervisorCore>,
        id: TaskId,
        slot_name: Arc<str>,
    ) {
        TrackedOperations::push(removals, async move {
            let decision = supervisor.remove(id).await;
            RemovalResult {
                id,
                slot_name,
                decision,
            }
        });
    }

    /// Failure reporting for a matching current-owner removal.
    ///
    /// Stale results are ignored.
    /// Success leaves slot ownership in place until physical completion.
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
