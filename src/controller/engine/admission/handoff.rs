//! Moves an idle slot into runtime registry admission.
//!
//! `placement` and `advance` call this module after selecting the next slot owner.
//! It tries to commit the task's stable identity, name, specification, and optional outcome sender to the registry Add command.
//!
//! A full registry queue starts a controller-owned capacity wait when admission limits allow it.
//! The controller retains the task and watcher and keeps the slot `Admitting`.
//! Limit failures and other errors before commit return the intact handoff for rejection and cleanup.

use std::sync::Arc;

use tokio::time::Instant;

use crate::{RuntimeError, core::SupervisorCore};

use super::{
    super::{
        Controller, TrackedOperations,
        state::{CapacityPending, PendingSubmission, SlotState},
    },
    cleanup::StartFailure,
};

impl Controller {
    /// Starts registry admission for one pending task in an idle slot.
    ///
    /// The slot becomes `Admitting` after the Add command commits or after the capacity wait is stored.
    /// A different pre-commit failure returns ownership.
    pub(super) fn start_in_slot(
        &self,
        sup: &Arc<SupervisorCore>,
        slot: &mut SlotState,
        slot_name: &Arc<str>,
        pending: PendingSubmission,
        operations: &mut TrackedOperations,
    ) -> Result<(), StartFailure> {
        assert!(slot.is_idle(), "start_in_slot requires an idle slot");
        let PendingSubmission {
            id,
            task_name,
            owned,
        } = pending;
        let done = self.state().watchers.remove(&id);
        match sup.add_task_with_id_watched(id, task_name, owned, done) {
            Ok((reply, completion)) => {
                let started = slot.begin_admission(id, Instant::now());
                debug_assert!(started);
                Self::track_admission(
                    &operations.admissions,
                    id,
                    Arc::clone(slot_name),
                    reply,
                    completion,
                );
                Ok(())
            }
            Err(mut uncommitted) => {
                if let Some(tx) = uncommitted.done.take() {
                    self.state().watchers.insert(id, tx);
                }
                if matches!(uncommitted.error, RuntimeError::CommandQueueFull) {
                    let crate::core::UncommittedWatchedAdd {
                        error: _,
                        name,
                        owned,
                        done,
                    } = *uncommitted;
                    debug_assert!(done.is_none(), "the watcher must remain controller-owned");
                    let waiting = CapacityPending {
                        slot_name: Arc::clone(slot_name),
                        pending: PendingSubmission::new(id, name, owned),
                    };
                    if let Err((limit, waiting)) = self.try_index_capacity_pending(id, waiting) {
                        let waiting = *waiting;
                        let PendingSubmission {
                            task_name: name,
                            owned,
                            ..
                        } = waiting.pending;
                        return Err(Box::new(crate::core::UncommittedWatchedAdd {
                            error: RuntimeError::ResourceLimitReached {
                                resource: "controller_pending",
                                limit,
                            },
                            name,
                            owned,
                            done,
                        }));
                    }
                    if let Err(limit) = operations.capacity.enqueue(id) {
                        let waiting = self.unindex_capacity_pending(id);
                        let PendingSubmission {
                            task_name: name,
                            owned,
                            ..
                        } = waiting.pending;
                        return Err(Box::new(crate::core::UncommittedWatchedAdd {
                            error: RuntimeError::ResourceLimitReached {
                                resource: "controller_admission",
                                limit,
                            },
                            name,
                            owned,
                            done,
                        }));
                    }
                    let started = slot.begin_admission(id, Instant::now());
                    debug_assert!(started);
                    Ok(())
                } else {
                    Err(uncommitted)
                }
            }
        }
    }
}
