//! Indexed queued-work lookup and concurrent registry fallback by identity.

use std::sync::Arc;

use tokio::sync::oneshot;

use crate::{
    RuntimeError,
    events::{Event, EventKind, RejectionKind},
    identity::TaskId,
};

use super::{Controller, ControllerWorkers, IdentityOperation, IdentityReply};

impl Controller {
    /// Starts one accepted identity operation after earlier controller commands have been handled.
    ///
    /// The controller checks and claims still-queued work through its reverse index, preserving command order.
    /// For every other ID, it starts a bounded registry-fallback worker.
    /// Those workers may finish concurrently and are not a global ordering barrier.
    /// The controller owns each worker; dropping the public caller cannot stop an accepted operation.
    pub(super) async fn handle_identity_operation(
        &self,
        id: TaskId,
        operation: IdentityOperation,
        reply: oneshot::Sender<Result<bool, RuntimeError>>,
        workers: &mut ControllerWorkers,
    ) {
        let reply = IdentityReply::new(reply);
        if self.is_shutting_down() {
            reply.send(Err(RuntimeError::ShuttingDown));
            return;
        }
        if self
            .remove_queued_submission(id, operation.request_reason(), workers)
            .await
        {
            reply.send(Ok(true));
            return;
        }
        if self.is_shutting_down() {
            reply.send(Err(RuntimeError::ShuttingDown));
            return;
        }

        let identity_limit = self.config.identity_operation_capacity().get();
        if workers.identity_operations.len() >= identity_limit {
            reply.send(Err(RuntimeError::ResourceLimitReached {
                resource: "controller_identity_operations",
                limit: identity_limit,
            }));
            return;
        }

        let Some(supervisor) = self.supervisor.upgrade() else {
            reply.send(Err(RuntimeError::ShuttingDown));
            return;
        };

        ControllerWorkers::push(&workers.identity_operations, async move {
            let result = match operation {
                IdentityOperation::Remove => supervisor.remove(id).await,
                IdentityOperation::TryRemove => supervisor.try_remove(id).await,
                IdentityOperation::Cancel => supervisor.cancel(id).await,
                IdentityOperation::TryCancel => supervisor.try_cancel(id).await,
                IdentityOperation::CancelWithTimeout(wait_for) => {
                    supervisor.cancel_with_timeout(id, wait_for).await
                }
                IdentityOperation::TryCancelWithTimeout(wait_for) => {
                    supervisor.try_cancel_with_timeout(id, wait_for).await
                }
            };
            reply.send(result);
        });
    }

    /// Removes one queued, not-yet-admitted submission by identity.
    ///
    /// Returns `true` only when this call claimed the queued submission.
    /// A claimed watched submission resolves as `Rejected { kind: RemovedFromQueue, .. }` because its task body never ran.
    pub(super) async fn remove_queued_submission(
        &self,
        id: TaskId,
        request_reason: Option<&'static str>,
        workers: &mut ControllerWorkers,
    ) -> bool {
        if let Some(super::MetadataCancellation {
            pending,
            done,
            discarded,
            unblocked,
        }) = self.cancel_metadata_pending(id)
        {
            pending.cancel.cancel();
            self.bus.publish_lazy(|| {
                let mut request = Event::new(EventKind::TaskRemoveRequested).with_id(id);
                if let Some(task) = &pending.event_task {
                    request = request.with_task(Arc::clone(task));
                }
                if let Some(reason) = request_reason {
                    request = request.with_reason(reason);
                }
                request
            });
            self.bus.publish_lazy(|| {
                let mut event = Event::new(EventKind::ControllerRejected)
                    .with_id(id)
                    .with_rejection_kind(RejectionKind::RemovedFromQueue)
                    .with_reason(crate::reasons::REMOVED_FROM_QUEUE);
                if let Some(task) = pending.event_task {
                    event = event.with_task(task);
                }
                event
            });
            let terminal = Self::send_rejected(
                done,
                RejectionKind::RemovedFromQueue,
                crate::reasons::REMOVED_FROM_QUEUE,
            );
            drop(terminal);
            drop(discarded);
            self.apply_metadata_results(unblocked, workers).await;
            return true;
        }
        let route = {
            let state = self.state();
            state
                .capacity_pending
                .get(&id)
                .map(|entry| (Arc::clone(&entry.slot_name), true))
                .or_else(|| {
                    state
                        .queued_slots
                        .get(&id)
                        .cloned()
                        .map(|slot| (slot, false))
                })
        };
        let Some((slot_name, capacity_pending)) = route else {
            return false;
        };
        let Some(slot_arc) = self.slot(&slot_name) else {
            if capacity_pending && let Some((waiting, done)) = self.claim_capacity_pending(id) {
                let cancelled = workers.capacity.cancel(id);
                debug_assert!(cancelled);
                self.bus.publish_lazy(|| {
                    let mut request = Event::new(EventKind::TaskRemoveRequested)
                        .with_task(Arc::clone(&waiting.pending.task_name))
                        .with_id(id);
                    if let Some(reason) = request_reason {
                        request = request.with_reason(reason);
                    }
                    request
                });
                self.bus.publish_lazy(|| {
                    Event::new(EventKind::ControllerRejected)
                        .with_task(Arc::clone(&waiting.slot_name))
                        .with_id(id)
                        .with_rejection_kind(RejectionKind::RemovedFromQueue)
                        .with_reason(crate::reasons::REMOVED_FROM_QUEUE)
                });
                let terminal = Self::send_rejected(
                    done,
                    RejectionKind::RemovedFromQueue,
                    crate::reasons::REMOVED_FROM_QUEUE,
                );
                self.drop_pending_submission(waiting.pending, terminal);
                return true;
            }
            self.state().queued_slots.remove(&id);
            return false;
        };

        let mut slot = slot_arc.lock().await;
        if self.is_shutting_down() {
            return false;
        }
        if let Some((waiting, done)) = self.claim_capacity_pending(id) {
            let cancelled = workers.capacity.cancel(id);
            debug_assert!(
                cancelled,
                "capacity-pending controller state must own one pump waiter"
            );
            self.bus.publish_lazy(|| {
                let mut request = Event::new(EventKind::TaskRemoveRequested)
                    .with_task(Arc::clone(&waiting.pending.task_name))
                    .with_id(id);
                if let Some(reason) = request_reason {
                    request = request.with_reason(reason);
                }
                request
            });
            self.bus.publish_lazy(|| {
                Event::new(EventKind::ControllerRejected)
                    .with_task(Arc::clone(&slot_name))
                    .with_id(id)
                    .with_rejection_kind(RejectionKind::RemovedFromQueue)
                    .with_reason(crate::reasons::REMOVED_FROM_QUEUE)
            });
            let terminal = Self::send_rejected(
                done,
                RejectionKind::RemovedFromQueue,
                crate::reasons::REMOVED_FROM_QUEUE,
            );
            let cleared = slot.reject_admission(id);
            debug_assert!(cleared);
            let deferred_drops = if let Some(supervisor) = self.supervisor.upgrade() {
                self.start_next_from_queue(&supervisor, &mut slot, &slot_name, workers)
            } else {
                Vec::new()
            };
            self.gc_if_idle(&slot_name, slot);
            self.drop_pending_submission(waiting.pending, terminal);
            self.drop_pending_submissions(deferred_drops);
            return true;
        }
        let Some(position) = slot.queue.iter().position(|pending| pending.id == id) else {
            self.state().queued_slots.remove(&id);
            return false;
        };
        self.bus.publish_lazy(|| {
            let mut request = Event::new(EventKind::TaskRemoveRequested)
                .with_task(Arc::clone(&slot.queue[position].task_name))
                .with_id(id);
            if let Some(reason) = request_reason {
                request = request.with_reason(reason);
            }
            request
        });
        let removed = self
            .remove_queued_at(&mut slot, position)
            .expect("the queued submission position was checked above");
        self.bus.publish_lazy(|| {
            Event::new(EventKind::ControllerRejected)
                .with_task(Arc::clone(&slot_name))
                .with_id(id)
                .with_rejection_kind(RejectionKind::RemovedFromQueue)
                .with_reason(crate::reasons::REMOVED_FROM_QUEUE)
        });
        let terminal = self.finalize_rejected(
            id,
            RejectionKind::RemovedFromQueue,
            crate::reasons::REMOVED_FROM_QUEUE,
        );
        self.gc_if_idle(&slot_name, slot);
        self.drop_pending_submission(removed, terminal);
        true
    }
}
