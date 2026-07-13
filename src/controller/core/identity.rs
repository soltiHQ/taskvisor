//! Ordered queued-work lookup and concurrent registry fallback by identity.

use std::sync::Arc;

use tokio::{sync::oneshot, task::JoinSet};

use crate::{
    RuntimeError,
    events::{Event, EventKind},
    identity::TaskId,
};

use super::{Controller, IdentityOperation, IdentityReply};

impl Controller {
    /// Starts one accepted identity operation after earlier controller commands have been handled.
    ///
    /// The controller checks and claims still-queued work inline, preserving command order.
    /// For every other ID, it starts a bounded registry-fallback worker.
    /// Those workers may finish concurrently and are not a global ordering barrier.
    /// The controller owns each worker; dropping the public caller cannot stop an accepted operation.
    pub(super) async fn handle_identity_operation(
        &self,
        id: TaskId,
        operation: IdentityOperation,
        reply: oneshot::Sender<Result<bool, RuntimeError>>,
        identity_operations: &mut JoinSet<()>,
    ) {
        let reply = IdentityReply::new(reply);
        if self.is_shutting_down() {
            reply.send(Err(RuntimeError::ShuttingDown));
            return;
        }
        if self
            .remove_queued_submission(id, operation.request_reason())
            .await
        {
            reply.send(Ok(true));
            return;
        }
        if self.is_shutting_down() {
            reply.send(Err(RuntimeError::ShuttingDown));
            return;
        }

        let Some(supervisor) = self.supervisor.upgrade() else {
            reply.send(Err(RuntimeError::ShuttingDown));
            return;
        };

        identity_operations.spawn(async move {
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
    /// A claimed watched submission resolves as `Rejected("removed_from_queue")` because its task body never ran.
    async fn remove_queued_submission(
        &self,
        id: TaskId,
        request_reason: Option<&'static str>,
    ) -> bool {
        let slot_keys: Vec<Arc<str>> = self
            .slots
            .iter()
            .map(|entry| Arc::clone(entry.key()))
            .collect();

        for slot_name in slot_keys {
            let Some(slot_arc) = self.slots.get(&*slot_name).map(|e| e.clone()) else {
                continue;
            };
            let mut slot = slot_arc.lock().await;
            if self.is_shutting_down() {
                return false;
            }
            let Some(pos) = slot.queue.iter().position(|(qid, _)| *qid == id) else {
                continue;
            };
            let task_name: Arc<str> = Arc::from(slot.queue[pos].1.name());
            let mut request = Event::new(EventKind::TaskRemoveRequested)
                .with_task(task_name)
                .with_id(id);
            if let Some(reason) = request_reason {
                request = request.with_reason(reason);
            }
            self.bus.publish(request);
            drop(
                slot.queue
                    .remove(pos)
                    .expect("the queued submission position was checked above"),
            );
            self.bus.publish(
                Event::new(EventKind::ControllerRejected)
                    .with_task(Arc::clone(&slot_name))
                    .with_id(id)
                    .with_reason(crate::reasons::REMOVED_FROM_QUEUE),
            );
            self.finalize_rejected(id, crate::reasons::REMOVED_FROM_QUEUE);
            self.gc_if_idle(&slot_name, slot);
            return true;
        }
        false
    }
}
