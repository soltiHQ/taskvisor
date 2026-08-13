//! Slot owner phases and pending work for one admission lane.
//!
//! `placement` starts admissions and replacement transitions. `results` applies
//! direct registry decisions and physical completion. `snapshot` reads the same
//! phases for diagnostics.
//!
//! ```text
//! Idle ──► Admitting
//!           ├── registry rejects ──► Idle
//!           ├── registry accepts ──► Running
//!           │                         ├── completion ──► Idle
//!           │                         └── replace ──► Terminating
//!           │                                               └── completion ──► Idle
//!           └── replace ──► CancelPendingAdmission
//!                              ├── registry rejects ──► Idle
//!                              └── registry accepts ──► Terminating
//!                                                          │
//!                                                          └── completion ──► Idle
//! ```
//!
//! `Terminating` returns to `Idle` only after physical completion.
//! Every occupied phase carries its owner's [`TaskId`].
//! Registry and completion transitions ignore stale task identities.

use std::{collections::VecDeque, sync::Arc};

use tokio::time::Instant;

use crate::{TaskSpec, core::deferred_drop::OwnedTask, identity::TaskId};

/// A task owned by the controller while it waits for registry admission.
pub(in crate::controller::engine) struct PendingSubmission {
    /// Stable identity used by the queue and runtime registry.
    pub(in crate::controller::engine) id: TaskId,
    /// Immutable name used for runtime registration.
    pub(in crate::controller::engine) task_name: Arc<str>,
    /// Runtime task specification coupled to reserved cleanup ownership.
    pub(in crate::controller::engine) owned: OwnedTask<TaskSpec>,
}

impl PendingSubmission {
    /// Creates a controller-owned pending submission.
    pub(in crate::controller::engine) fn new(
        id: TaskId,
        task_name: Arc<str>,
        owned: OwnedTask<TaskSpec>,
    ) -> Self {
        Self {
            id,
            task_name,
            owned,
        }
    }

    /// Returns the retained task specification in tests.
    #[cfg(test)]
    pub(in crate::controller::engine) fn task_spec(&self) -> &TaskSpec {
        &self.owned.value
    }
}

/// Owner phase and pending queue for one controller slot.
pub(in crate::controller::engine) struct SlotState {
    /// Current slot lifecycle phase.
    phase: SlotPhase,

    /// Pending submissions in admission order.
    ///
    /// The front item is next after the current owner is cleared.
    pub(in crate::controller::engine) queue: VecDeque<PendingSubmission>,
}

/// Current owner phase of one controller slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::controller::engine) enum SlotPhase {
    /// No current owner.
    Idle,

    /// Registry admission has started but has no final Add decision.
    ///
    /// The controller may still be waiting for bounded registry command
    /// capacity, or the registration request is waiting for its reply.
    Admitting {
        /// Task identity waiting for registry admission.
        owner: TaskId,
        /// Time when admission started.
        since: Instant,
    },

    /// Replacement was requested before the registry Add decision.
    ///
    /// The replacement path waits for the registry decision.
    /// It orders removal only after the registry accepts the task.
    /// Public snapshots expose this phase as `Terminating`.
    CancelPendingAdmission {
        /// Task identity waiting for its registry decision.
        owner: TaskId,
        /// Time when replacement was requested.
        requested_at: Instant,
    },

    /// The registry accepted the task and the slot still owns it.
    Running {
        /// Registered task identity.
        owner: TaskId,
        /// Time when the registry accepted the task.
        started_at: Instant,
    },

    /// Owner removal has started but physical completion is still pending.
    Terminating {
        /// Registered task identity being retired.
        owner: TaskId,
        /// Time when replacement requested removal.
        requested_at: Instant,
    },
}

/// Effect produced when a busy slot receives a replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::controller::engine) enum ReplaceAction {
    /// The accepted owner can be removed immediately.
    RemoveNow(TaskId),
    /// The registration reply must arrive before removal can be ordered.
    WaitForAdmission,
    /// Replacement was already recorded; removal is pending or already requested.
    AlreadyRequested,
    /// The slot was idle. Replacement policy does not apply.
    Idle,
}

/// Effect produced by one authoritative successful registration reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::controller::engine) enum AdmissionTransition {
    /// Admission completed and the slot is now running.
    Running,
    /// Admission completed after an early replacement; removal must now be ordered.
    RemoveNow(TaskId),
    /// The reply does not belong to the current admission phase.
    Stale,
}

impl SlotPhase {
    /// Current owner identity, absent only for Idle.
    pub(in crate::controller::engine) fn owner_id(self) -> Option<TaskId> {
        match self {
            Self::Idle => None,
            Self::Admitting { owner, .. }
            | Self::CancelPendingAdmission { owner, .. }
            | Self::Running { owner, .. }
            | Self::Terminating { owner, .. } => Some(owner),
        }
    }

    /// Returns the diagnostic status label for this phase.
    pub(in crate::controller::engine) fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Admitting { .. } => "admitting",
            Self::Running { .. } => "running",
            Self::CancelPendingAdmission { .. } | Self::Terminating { .. } => "terminating",
        }
    }
}

impl SlotState {
    /// Creates an idle slot with no owner and no queued submissions.
    pub(in crate::controller::engine) fn new() -> Self {
        Self {
            phase: SlotPhase::Idle,
            queue: VecDeque::new(),
        }
    }

    /// Returns the current lifecycle phase.
    pub(in crate::controller::engine) fn phase(&self) -> SlotPhase {
        self.phase
    }

    /// Returns the current owner identity when the slot is occupied.
    pub(in crate::controller::engine) fn owner_id(&self) -> Option<TaskId> {
        self.phase.owner_id()
    }

    /// Returns whether the slot has no current owner.
    pub(in crate::controller::engine) fn is_idle(&self) -> bool {
        matches!(self.phase, SlotPhase::Idle)
    }

    /// Returns the phase label used in busy-slot rejection details.
    pub(in crate::controller::engine) fn status_label(&self) -> &'static str {
        self.phase.label()
    }

    /// Starts admission when the slot is idle.
    ///
    /// Returns `false` without changing an occupied slot.
    pub(in crate::controller::engine) fn begin_admission(
        &mut self,
        owner: TaskId,
        since: Instant,
    ) -> bool {
        if !self.is_idle() {
            return false;
        }
        self.phase = SlotPhase::Admitting { owner, since };
        true
    }

    /// Applies replacement intent and returns the required removal action.
    pub(in crate::controller::engine) fn request_replacement(
        &mut self,
        requested_at: Instant,
    ) -> ReplaceAction {
        match self.phase {
            SlotPhase::Idle => ReplaceAction::Idle,
            SlotPhase::Admitting { owner, .. } => {
                self.phase = SlotPhase::CancelPendingAdmission {
                    owner,
                    requested_at,
                };
                ReplaceAction::WaitForAdmission
            }
            SlotPhase::Running { owner, .. } => {
                self.phase = SlotPhase::Terminating {
                    owner,
                    requested_at,
                };
                ReplaceAction::RemoveNow(owner)
            }
            SlotPhase::CancelPendingAdmission { .. } | SlotPhase::Terminating { .. } => {
                ReplaceAction::AlreadyRequested
            }
        }
    }

    /// Applies a successful registry Add decision for the matching owner.
    pub(in crate::controller::engine) fn confirm_admission(
        &mut self,
        owner: TaskId,
        started_at: Instant,
    ) -> AdmissionTransition {
        match self.phase {
            SlotPhase::Admitting { owner: current, .. } if current == owner => {
                self.phase = SlotPhase::Running { owner, started_at };
                AdmissionTransition::Running
            }
            SlotPhase::CancelPendingAdmission {
                owner: current,
                requested_at,
            } if current == owner => {
                self.phase = SlotPhase::Terminating {
                    owner,
                    requested_at,
                };
                AdmissionTransition::RemoveNow(owner)
            }
            _ => AdmissionTransition::Stale,
        }
    }

    /// Clears a matching admission after the registry rejects it.
    ///
    /// Returns `false` without changing a stale or non-admitting owner.
    pub(in crate::controller::engine) fn reject_admission(&mut self, owner: TaskId) -> bool {
        let matches_current = matches!(
            self.phase,
            SlotPhase::Admitting { owner: current, .. }
                | SlotPhase::CancelPendingAdmission { owner: current, .. }
                if current == owner
        );
        if matches_current {
            self.phase = SlotPhase::Idle;
        }
        matches_current
    }

    /// Releases a matching accepted owner after physical completion.
    ///
    /// Returns `false` for a stale owner or a phase without an accepted task.
    pub(in crate::controller::engine) fn complete_owner(&mut self, owner: TaskId) -> bool {
        let matches_current = matches!(
            self.phase,
            SlotPhase::Running { owner: current, .. }
                | SlotPhase::Terminating { owner: current, .. }
                if current == owner
        );
        if matches_current {
            self.phase = SlotPhase::Idle;
        }
        matches_current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_slot_is_idle_with_empty_queue() {
        let slot = SlotState::new();
        assert_eq!(slot.phase(), SlotPhase::Idle);
        assert_eq!(slot.owner_id(), None);
        assert!(slot.queue.is_empty());
    }

    #[test]
    fn every_occupied_phase_carries_its_owner() {
        let owner = TaskId::next();
        let now = Instant::now();
        for phase in [
            SlotPhase::Admitting { owner, since: now },
            SlotPhase::CancelPendingAdmission {
                owner,
                requested_at: now,
            },
            SlotPhase::Running {
                owner,
                started_at: now,
            },
            SlotPhase::Terminating {
                owner,
                requested_at: now,
            },
        ] {
            assert_eq!(phase.owner_id(), Some(owner));
        }
        assert_eq!(SlotPhase::Idle.owner_id(), None);
    }

    #[test]
    fn early_replace_waits_for_admission_then_enters_real_termination() {
        let owner = TaskId::next();
        let now = Instant::now();
        let mut slot = SlotState::new();
        assert!(slot.begin_admission(owner, now));
        assert_eq!(
            slot.request_replacement(now),
            ReplaceAction::WaitForAdmission
        );
        assert!(matches!(
            slot.phase(),
            SlotPhase::CancelPendingAdmission { owner: id, .. } if id == owner
        ));
        assert!(
            !slot.complete_owner(owner),
            "completion cannot release an admission that is still pending"
        );
        assert_eq!(
            slot.confirm_admission(owner, now),
            AdmissionTransition::RemoveNow(owner)
        );
        assert!(matches!(
            slot.phase(),
            SlotPhase::Terminating { owner: id, .. } if id == owner
        ));
        assert!(slot.complete_owner(owner));
        assert!(slot.is_idle());
    }

    #[test]
    fn stale_results_do_not_mutate_current_owner() {
        let owner = TaskId::next();
        let stale = TaskId::next();
        let now = Instant::now();
        let mut slot = SlotState::new();
        assert!(slot.begin_admission(owner, now));
        assert_eq!(
            slot.confirm_admission(stale, now),
            AdmissionTransition::Stale
        );
        assert!(!slot.reject_admission(stale));
        assert_eq!(slot.owner_id(), Some(owner));
        assert!(matches!(slot.phase(), SlotPhase::Admitting { .. }));
    }

    #[test]
    fn queue_push_pop_fifo() {
        let mut slot = SlotState::new();
        let pending = |name: &str| {
            let task_spec = make_spec(name);
            let retained = task_spec.task().clone();
            let reservation = crate::core::deferred_drop::test_reservation();
            PendingSubmission::new(
                TaskId::next(),
                Arc::from(name),
                OwnedTask::new(task_spec, retained, reservation),
            )
        };

        slot.queue.push_back(pending("a"));
        slot.queue.push_back(pending("b"));
        slot.queue.push_back(pending("c"));

        assert_eq!(slot.queue.len(), 3);
        assert_eq!(slot.queue.pop_front().unwrap().task_spec().name(), "a");
        assert_eq!(slot.queue.pop_front().unwrap().task_spec().name(), "b");
        assert_eq!(slot.queue.pop_front().unwrap().task_spec().name(), "c");
        assert!(slot.queue.is_empty());
    }

    fn make_spec(name: &str) -> TaskSpec {
        use crate::TaskContext;
        use crate::{TaskFn, TaskRef};

        let task: TaskRef = TaskFn::arc(|_ctx: TaskContext| async { Ok(()) });
        TaskSpec::once(name, task)
    }
}
