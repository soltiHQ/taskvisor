use std::{collections::VecDeque, time::Instant};

use crate::TaskSpec;

/// State of a single task slot.
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
