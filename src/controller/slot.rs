//! Internal typed slot state for the controller.
//!
//! A slot is one controller admission lane.
//! It may have one current owner and a deque of pending submissions.
//! `Queue` submissions keep FIFO order; `Replace` may supersede the head.
//!
//! `SlotPhase` owns the current `TaskId` in every occupied phase; an idle slot cannot retain an owner and an occupied slot cannot exist without one.
//! Transition methods reject stale identities without mutating the current owner.

use std::collections::VecDeque;

use tokio::time::Instant;

use crate::TaskSpec;
use crate::identity::TaskId;

/// Mutable state for one controller slot.
pub(super) struct SlotState {
    phase: SlotPhase,

    /// Pending submissions for this slot.
    ///
    /// The front item is next after the current admission fails or after terminal cleanup of an accepted owner.
    pub(super) queue: VecDeque<(TaskId, TaskSpec)>,
}

/// Internal lifecycle phase of one controller slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SlotPhase {
    /// No current owner.
    Idle,

    /// The Add command was committed, but the registry decision is still pending.
    Admitting { owner: TaskId, since: Instant },

    /// Replacement was requested while Add was pending.
    ///
    /// The replacement path waits for the registry decision.
    /// It orders removal only after the registry accepts the Add.
    /// Public snapshots expose this phase as `Terminating`.
    CancelPendingAdmission {
        owner: TaskId,
        requested_at: Instant,
    },

    /// The registry accepted the task, and terminal cleanup has not yet been applied by the controller.
    Running { owner: TaskId, started_at: Instant },

    /// A replacement-driven runtime removal request has started, and terminal registry cleanup has not yet been applied.
    Terminating {
        owner: TaskId,
        requested_at: Instant,
    },
}

/// Effect produced when a busy slot receives a replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReplaceAction {
    /// The accepted owner can be removed immediately.
    RemoveNow(TaskId),
    /// The Add reply must arrive before removal can be ordered.
    WaitForAdmission,
    /// Replacement was already recorded; removal is pending or already requested.
    AlreadyRequested,
    /// The slot was idle. Replacement policy does not apply.
    Idle,
}

/// Effect produced by one authoritative successful Add reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AdmissionTransition {
    /// Admission completed and the slot is now running.
    Running,
    /// Admission completed after an early replacement; removal must now be ordered.
    RemoveNow(TaskId),
    /// The reply does not belong to the current admission phase.
    Stale,
}

impl SlotPhase {
    /// Current owner identity, absent only for Idle.
    pub(super) fn owner_id(self) -> Option<TaskId> {
        match self {
            Self::Idle => None,
            Self::Admitting { owner, .. }
            | Self::CancelPendingAdmission { owner, .. }
            | Self::Running { owner, .. }
            | Self::Terminating { owner, .. } => Some(owner),
        }
    }

    /// Stable external diagnostic label.
    pub(super) fn label(self) -> &'static str {
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
    pub(super) fn new() -> Self {
        Self {
            phase: SlotPhase::Idle,
            queue: VecDeque::new(),
        }
    }

    pub(super) fn phase(&self) -> SlotPhase {
        self.phase
    }

    pub(super) fn owner_id(&self) -> Option<TaskId> {
        self.phase.owner_id()
    }

    pub(super) fn is_idle(&self) -> bool {
        matches!(self.phase, SlotPhase::Idle)
    }

    pub(super) fn status_label(&self) -> &'static str {
        self.phase.label()
    }

    /// Starts a new admission only when the slot is idle.
    pub(super) fn begin_admission(&mut self, owner: TaskId, since: Instant) -> bool {
        if !self.is_idle() {
            return false;
        }
        self.phase = SlotPhase::Admitting { owner, since };
        true
    }

    /// Records replacement intent and tells the engine when removal may be ordered.
    pub(super) fn request_replacement(&mut self, requested_at: Instant) -> ReplaceAction {
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

    /// Applies a successful Add reply for the current owner.
    pub(super) fn confirm_admission(
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

    /// Applies a rejected Add reply only to the matching pending admission.
    pub(super) fn reject_admission(&mut self, owner: TaskId) -> bool {
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

    /// Releases the matching owner after terminal registry cleanup.
    pub(super) fn complete_owner(&mut self, owner: TaskId) -> bool {
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

        slot.queue.push_back((TaskId::next(), make_spec("a")));
        slot.queue.push_back((TaskId::next(), make_spec("b")));
        slot.queue.push_back((TaskId::next(), make_spec("c")));

        assert_eq!(slot.queue.len(), 3);
        assert_eq!(slot.queue.pop_front().unwrap().1.name(), "a");
        assert_eq!(slot.queue.pop_front().unwrap().1.name(), "b");
        assert_eq!(slot.queue.pop_front().unwrap().1.name(), "c");
        assert!(slot.queue.is_empty());
    }

    fn make_spec(name: &str) -> TaskSpec {
        use crate::TaskContext;
        use crate::{BackoffPolicy, RestartPolicy, TaskFn, TaskRef};

        let task: TaskRef = TaskFn::arc(name, |_ctx: TaskContext| async { Ok(()) });
        TaskSpec::new(task, RestartPolicy::Never, BackoffPolicy::default(), None)
    }
}
