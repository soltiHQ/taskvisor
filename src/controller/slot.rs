//! Internal slot state for the controller.
//!
//! A slot is one controller admission lane.
//! It may have one current owner and a FIFO queue of submissions waiting behind it.
//!
//! This module only stores slot state.
//! The transition logic lives in `controller::core`.

use std::collections::VecDeque;

use tokio::time::Instant;

use crate::TaskSpec;
use crate::identity::TaskId;

/// Mutable state for one controller slot.
///
/// Invariants expected by the controller:
/// - `queue` contains only submissions that have not been handed to the runtime yet.
/// - `Admitting`, `Running`, and `Terminating` slots normally have a `running_id`.
/// - `queue` does not include the current slot owner.
/// - `Idle` slots have no `running_id`.
///
/// The controller stores each `SlotState` behind a per-slot mutex.
pub(super) struct SlotState {
    /// Current lifecycle state of the slot owner.
    pub status: SlotStatus,

    /// Runtime identity of the current slot owner.
    ///
    /// Used to remove the task and to correlate runtime events back to this slot.
    pub running_id: Option<TaskId>,

    /// Pending submissions for this slot.
    ///
    /// The front item is the next submission to admit after the current owner is removed.
    pub queue: VecDeque<(TaskId, TaskSpec)>,
}

/// Internal lifecycle state of one controller slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SlotStatus {
    /// No current owner.
    Idle,

    /// The controller sent the task to the runtime, but the registry has not confirmed it yet.
    ///
    /// The slot becomes `Running` when the direct registry Add reply succeeds.
    /// It becomes `Idle` or advances to the next queued task when that reply rejects the Add.
    Admitting {
        /// Time when admission was requested.
        ///
        /// Used for controller snapshots.
        since: Instant,
    },

    /// The registry accepted the task through its direct Add reply.
    Running {
        /// Time when the slot entered `Running`.
        ///
        /// Used for controller snapshots.
        started_at: Instant,
    },

    /// The current owner is being removed.
    ///
    /// The slot waits for reliable terminal registry cleanup before it starts the next queued
    /// submission.
    Terminating {
        /// Time when removal was requested.
        ///
        /// Used for controller snapshots.
        cancelled_at: Instant,
    },
}

impl SlotStatus {
    /// Returns a short stable label for diagnostics.
    ///
    /// This avoids formatting internal timestamps into event reasons.
    pub fn label(&self) -> &'static str {
        match self {
            SlotStatus::Idle => "idle",
            SlotStatus::Admitting { .. } => "admitting",
            SlotStatus::Running { .. } => "running",
            SlotStatus::Terminating { .. } => "terminating",
        }
    }
}

impl SlotState {
    /// Creates an idle slot with no owner and no queued submissions.
    pub fn new() -> Self {
        Self {
            status: SlotStatus::Idle,
            running_id: None,
            queue: VecDeque::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_slot_is_idle_with_empty_queue() {
        let slot = SlotState::new();
        assert_eq!(slot.status, SlotStatus::Idle);
        assert!(slot.running_id.is_none());
        assert!(slot.queue.is_empty());
    }

    #[test]
    fn only_idle_is_treated_as_a_free_slot() {
        let now = Instant::now();
        assert!(matches!(SlotStatus::Idle, SlotStatus::Idle));
        for occupied in [
            SlotStatus::Admitting { since: now },
            SlotStatus::Running { started_at: now },
            SlotStatus::Terminating { cancelled_at: now },
        ] {
            assert!(
                !matches!(occupied, SlotStatus::Idle),
                "{} must count as occupied, not free",
                occupied.label()
            );
        }
    }

    #[test]
    fn labels_are_stable_and_do_not_include_timestamps() {
        let now = Instant::now();

        assert_eq!(SlotStatus::Idle.label(), "idle");
        assert_eq!(SlotStatus::Admitting { since: now }.label(), "admitting");
        assert_eq!(SlotStatus::Running { started_at: now }.label(), "running");
        assert_eq!(
            SlotStatus::Terminating { cancelled_at: now }.label(),
            "terminating"
        );
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
