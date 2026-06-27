//! Read-only views of controller slot state.

use std::sync::Arc;
use std::time::Duration;

use crate::identity::TaskId;

/// Status discriminant of a controller slot.
///
/// An `Instant`-free public mirror of the controller's internal slot status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SlotStatusKind {
    /// No task running; ready to accept submissions.
    Idle,
    /// A task was admitted and the slot awaits its `TaskAdded` confirmation.
    Admitting,
    /// A task is currently running in the slot.
    Running,
    /// The occupying task is being cancelled (awaiting `TaskRemoved`).
    Terminating,
}

/// Point-in-time view of a single controller slot.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SlotView {
    /// Slot key (`TaskSpec::slot`, defaulting to the task name).
    pub slot: Arc<str>,
    /// Current status.
    pub status: SlotStatusKind,
    /// Identity of the task currently occupying the slot, if any.
    pub running: Option<TaskId>,
    /// Number of submissions waiting in this slot's queue.
    pub queue_depth: usize,
    /// How long the slot has been in its current status (`0` for `Idle`).
    pub status_for: Duration,
}

/// Point-in-time snapshot of the controller's slots.
///
/// **Not globally atomic**: slots are sampled one at a time, exactly like [`SupervisorHandle::list`](crate::SupervisorHandle::list).
/// Treat the figures as gauges.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ControllerSnapshot {
    /// All currently tracked slots, sorted by slot key.
    pub slots: Vec<SlotView>,
}

impl ControllerSnapshot {
    /// Number of slots currently `Running`.
    #[must_use]
    pub fn running_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.status == SlotStatusKind::Running)
            .count()
    }

    /// Total submissions queued across all slots.
    #[must_use]
    pub fn total_queued(&self) -> usize {
        self.slots.iter().map(|s| s.queue_depth).sum()
    }

    /// View of a specific slot, if currently tracked.
    #[must_use]
    pub fn slot(&self, name: &str) -> Option<&SlotView> {
        self.slots.iter().find(|s| &*s.slot == name)
    }

    /// Number of tracked slots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether no slots are tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}
