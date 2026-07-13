//! Tracking for registry admission, terminal completion, and removal workers.

use std::sync::Arc;

use tokio::task::JoinSet;

use crate::{
    RuntimeError,
    core::{AddReplyRx, RemovalCompletion, SupervisorCore},
    events::{Event, EventKind},
    identity::TaskId,
};

use super::{AdmissionResult, CompletionResult, Controller, RemovalResult};

impl Controller {
    /// Tracks a committed Add command until its direct registry reply arrives.
    pub(super) fn track_admission(
        admissions: &mut JoinSet<AdmissionResult>,
        id: TaskId,
        slot_name: Arc<str>,
        reply: AddReplyRx,
        completion: RemovalCompletion,
    ) {
        admissions.spawn(async move {
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

    /// Tracks one accepted task until terminal registry cleanup is committed.
    pub(super) fn track_completion(
        completions: &mut JoinSet<CompletionResult>,
        id: TaskId,
        slot_name: Arc<str>,
        completion: RemovalCompletion,
    ) {
        completions.spawn(async move {
            completion.wait().await;
            CompletionResult { id, slot_name }
        });
    }

    /// Orders one runtime removal without blocking the controller loop on registry backpressure.
    pub(super) fn track_removal(
        removals: &mut JoinSet<RemovalResult>,
        supervisor: Arc<SupervisorCore>,
        id: TaskId,
        slot_name: Arc<str>,
    ) {
        removals.spawn(async move {
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
    /// Successful claims and already-removing tasks both finish through the reliable completion
    /// signal. An error is diagnostic only; shutdown cleanup remains authoritative.
    pub(super) async fn handle_removal_result(&self, result: RemovalResult) {
        let Some(slot) = self
            .slots
            .get(&*result.slot_name)
            .map(|entry| entry.clone())
        else {
            return;
        };
        if slot.lock().await.owner_id() != Some(result.id) {
            return;
        }
        if let Err(error) = result.decision {
            self.bus.publish(
                Event::new(EventKind::ControllerRejected)
                    .with_task(result.slot_name)
                    .with_id(result.id)
                    .with_reason(format!("remove_failed: {error}")),
            );
        }
    }
}
