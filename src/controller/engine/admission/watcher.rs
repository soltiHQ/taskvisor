//! Guards task and outcome ownership before admission commits.
//!
//! The guard starts with local task ownership and an optional watched-outcome sender.
//! Parking transfers the sender to controller state.
//! Committing disarms the guard after a slot queue, capacity wait, or registry command owns the task.
//!
//! Rejection resolves the sender once.
//! Dropping an uncommitted guard reports interrupted admission and routes any local task value
//! through reserved cleanup.

use std::sync::Arc;

use crate::{
    core::{OutcomeTx, TaskOutcome, deferred_drop::OwnedTask},
    events::{Event, EventKind, RejectionKind},
    identity::TaskId,
};

use super::super::{Controller, state::PendingSubmission};

/// Preserves one task and optional watcher until admission ownership commits.
pub(super) struct AdmissionWatcher<'a> {
    /// Controller that owns parked outcomes and diagnostics.
    controller: &'a Controller,
    /// Task identity tracked by the transaction.
    id: TaskId,
    /// Slot label used by rejection events.
    event_task: Option<Arc<str>>,
    /// Current ownership stage.
    state: AdmissionWatcherState,
    /// User task ownership before conversion to pending `TaskSpec`.
    owned: Option<OwnedTask<crate::ControllerSpec>>,
}

/// Ownership stage of one guarded admission.
enum AdmissionWatcherState {
    /// Task and optional outcome sender remain local to the guard.
    ///
    /// No shared controller index owns the sender yet.
    Local(Option<OutcomeTx>),
    /// The optional outcome sender is parked in controller state.
    Parked,
    /// Queue, capacity-wait, or registry ownership has committed.
    Committed,
}

impl<'a> AdmissionWatcher<'a> {
    pub(super) fn new(
        controller: &'a Controller,
        id: TaskId,
        owned: OwnedTask<crate::ControllerSpec>,
        done: Option<OutcomeTx>,
        event_task: Option<Arc<str>>,
    ) -> Self {
        Self {
            controller,
            id,
            event_task,
            state: AdmissionWatcherState::Local(done),
            owned: Some(owned),
        }
    }

    /// Transfer of local task ownership into a pending submission.
    pub(super) fn take_pending(&mut self, id: TaskId, task_name: Arc<str>) -> PendingSubmission {
        let owned = self
            .owned
            .take()
            .expect("controller ownership is transferred once")
            .map(crate::ControllerSpec::into_task_spec);
        PendingSubmission::new(id, task_name, owned)
    }

    /// Reserved cleanup for user ownership that never committed.
    fn dispose_owned(&mut self, terminal: Option<TaskOutcome>) {
        let Some(owned) = self.owned.take() else {
            return;
        };
        let (spec, mut cleanup) = owned.into_parts();
        drop(spec);
        if let Some(terminal) = terminal {
            cleanup.attach_outcome(terminal);
        }
        cleanup.submit();
    }

    /// Transfer of the optional outcome sender into controller state.
    pub(super) fn park(&mut self) {
        let state = std::mem::replace(&mut self.state, AdmissionWatcherState::Committed);
        match state {
            AdmissionWatcherState::Local(Some(tx)) => {
                self.controller.state().watchers.insert(self.id, tx);
                self.state = AdmissionWatcherState::Parked;
            }
            AdmissionWatcherState::Local(None) => {
                self.state = AdmissionWatcherState::Parked;
            }
            AdmissionWatcherState::Committed => {}
            AdmissionWatcherState::Parked => {
                self.state = AdmissionWatcherState::Parked;
            }
        }
    }

    /// Fallback disarm after queue or admission ownership commits.
    pub(super) fn commit(&mut self) {
        debug_assert!(
            !matches!(self.state, AdmissionWatcherState::Local(Some(_))),
            "a watched admission must be parked before commit"
        );
        self.state = AdmissionWatcherState::Committed;
    }

    /// Single terminal rejection and fallback disarm.
    ///
    /// An undelivered outcome remains caller-owned after task ownership leaves the guard.
    fn reject(&mut self, kind: RejectionKind, reason: &str) -> Option<TaskOutcome> {
        let state = std::mem::replace(&mut self.state, AdmissionWatcherState::Committed);
        let undelivered = match state {
            AdmissionWatcherState::Local(Some(tx)) => tx
                .send(TaskOutcome::Rejected {
                    kind,
                    reason: Arc::from(reason),
                })
                .err(),
            AdmissionWatcherState::Parked => {
                self.controller.finalize_rejected(self.id, kind, reason)
            }
            AdmissionWatcherState::Local(None) | AdmissionWatcherState::Committed => None,
        };
        if self.owned.is_some() {
            self.dispose_owned(undelivered);
            None
        } else {
            undelivered
        }
    }

    /// Idempotent reporting and resolution of one controller-side rejection.
    pub(super) fn reject_with_event(
        &mut self,
        kind: RejectionKind,
        reason: &str,
    ) -> Option<TaskOutcome> {
        if matches!(self.state, AdmissionWatcherState::Committed) {
            return None;
        }
        self.controller.bus.publish_lazy(|| {
            let mut event = Event::new(EventKind::ControllerRejected)
                .with_id(self.id)
                .with_rejection_kind(kind)
                .with_reason(reason);
            if let Some(task) = &self.event_task {
                event = event.with_task(Arc::clone(task));
            }
            event
        });
        self.reject(kind, reason)
    }
}

impl Drop for AdmissionWatcher<'_> {
    fn drop(&mut self) {
        if matches!(self.state, AdmissionWatcherState::Committed) {
            return;
        }
        drop(self.reject_with_event(
            RejectionKind::AdmissionFailed,
            crate::reasons::CONTROLLER_ADMISSION_INTERRUPTED,
        ));
    }
}
