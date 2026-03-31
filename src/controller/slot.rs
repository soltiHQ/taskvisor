//! # Slot state for the [`Controller`](super::Controller).
//!
//! [`SlotState`] tracks the current status and pending queue for a single named slot.

use std::{collections::VecDeque, time::Instant};

use crate::TaskSpec;

/// State of a single task slot.
///
/// # Also
///
/// - [`AdmissionPolicy`](super::AdmissionPolicy) - determines how submissions interact with slot state
/// - [`Controller`](super::Controller) - owns and manages slot states
pub(super) struct SlotState {
    /// Current status (idle, running, or terminating).
    pub status: SlotStatus,

    /// Queue of pending tasks (FIFO order).
    pub queue: VecDeque<TaskSpec>,
}

/// Status of a task slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SlotStatus {
    /// No task running, ready to accept new submissions.
    Idle,

    /// Task currently running.
    Running {
        /// When the task started.
        started_at: Instant,
    },

    /// Task is being cancelled (waiting for TaskRemoved event).
    Terminating {
        /// When cancellation was requested.
        cancelled_at: Instant,
    },
}

impl SlotState {
    /// Creates a new idle slot.
    pub fn new() -> Self {
        Self {
            status: SlotStatus::Idle,
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
        assert!(slot.queue.is_empty());
    }

    #[test]
    fn idle_ne_running_ne_terminating() {
        let idle = SlotStatus::Idle;
        let now = Instant::now();
        let running = SlotStatus::Running { started_at: now };
        let terminating = SlotStatus::Terminating { cancelled_at: now };

        assert_ne!(idle, running);
        assert_ne!(idle, terminating);
        assert_ne!(running, terminating);
    }

    #[test]
    fn matches_idle_works_on_all_variants() {
        let now = Instant::now();
        assert!(matches!(SlotStatus::Idle, SlotStatus::Idle));
        assert!(!matches!(
            SlotStatus::Running { started_at: now },
            SlotStatus::Idle
        ));
        assert!(!matches!(
            SlotStatus::Terminating { cancelled_at: now },
            SlotStatus::Idle
        ));
    }

    #[test]
    fn queue_push_pop_fifo() {
        let mut slot = SlotState::new();

        let task_a = make_spec("a");
        let task_b = make_spec("b");
        let task_c = make_spec("c");

        slot.queue.push_back(task_a);
        slot.queue.push_back(task_b);
        slot.queue.push_back(task_c);

        assert_eq!(slot.queue.len(), 3);
        assert_eq!(slot.queue.pop_front().unwrap().name(), "a");
        assert_eq!(slot.queue.pop_front().unwrap().name(), "b");
        assert_eq!(slot.queue.pop_front().unwrap().name(), "c");
        assert!(slot.queue.is_empty());
    }

    fn make_spec(name: &str) -> TaskSpec {
        use crate::{BackoffPolicy, RestartPolicy, TaskFn, TaskRef};
        use tokio_util::sync::CancellationToken;

        let task: TaskRef = TaskFn::arc(name, |_ctx: CancellationToken| async { Ok(()) });
        TaskSpec::new(task, RestartPolicy::Never, BackoffPolicy::default(), None)
    }
}
