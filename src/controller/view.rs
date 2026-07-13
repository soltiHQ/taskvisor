//! # Read-only controller state
//!
//! This module exposes the public snapshot types returned by
//! [`SupervisorHandle::controller_snapshot`](crate::SupervisorHandle::controller_snapshot).
//!
//! Use snapshots for gauges, dashboards, health checks, and tests.
//! Callers do not need to parse best-effort events to read the current controller state.

use std::sync::Arc;
use std::time::Duration;

use crate::identity::TaskId;

/// Public status of one controller slot.
///
/// This is the public, `Instant`-free mirror of the controller's internal slot state.
/// Use a wildcard arm when matching this enum, because new states may be added later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SlotStatusKind {
    /// No current slot owner.
    Idle,
    /// The controller selected a submission and sent its registration request.
    ///
    /// Registry acceptance is still pending from the controller's point of view.
    Admitting,
    /// The registry accepted the task, and the controller has not yet applied its terminal cleanup signal.
    ///
    /// This does not prove that the task body is executing now.
    /// It may be waiting for the supervisor's global concurrency permit, sleeping between attempts, or finishing cleanup.
    Running,
    /// A replacement is waiting for the current submission to leave the slot.
    ///
    /// The controller is either waiting for a pending registration decision before it can order removal,
    /// or waiting for reliable terminal registry cleanup after removal was ordered.
    ///
    /// `Replace` enters this state.
    /// An ID-based `remove*` or `cancel*` request does not change the public status by itself.
    Terminating,
}

/// Point-in-time view of one controller slot.
///
/// Use `..` when matching this struct because more fields may be added.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SlotView {
    /// Slot key used for controller admission.
    ///
    /// This is [`ControllerSpec::slot_name`](crate::ControllerSpec::slot_name).
    /// It defaults to the task name, but may be overridden with [`ControllerSpec::with_slot`](crate::ControllerSpec::with_slot).
    pub slot: Arc<str>,

    /// Current controller status for this slot.
    pub status: SlotStatusKind,

    /// Identity of the current slot owner, if any.
    ///
    /// Present for `Admitting`, `Running`, and `Terminating`.
    /// Absent for `Idle`.
    ///
    /// In `Admitting`, this identity has been allocated for the controller submission but the controller has not received registry acceptance yet.
    /// The same can be true in `Terminating` when `Replace` arrived during admission.
    pub owner_id: Option<TaskId>,

    /// Number of pending submissions in this slot's queue.
    ///
    /// This does not include the current slot owner.
    /// It does include a replacement at the queue head.
    pub queue_depth: usize,

    /// How long the slot has been in its current status.
    ///
    /// This measures controller-state time, not task execution time.
    /// `Idle` reports `Duration::ZERO`.
    pub status_for: Duration,
}

/// Best-effort rolling snapshot of controller slots.
///
/// This is a read-side view for observability, not a lifecycle guarantee.
///
/// Each included slot is read consistently under its own lock, but the full snapshot is not globally atomic.
/// Taskvisor first captures known slot keys, then reads those slots one by one.
/// A removed slot may still appear if the snapshot found it before removal, and a newly created slot may be absent.
/// Treat all values as gauges, not as a transactional view.
///
/// Idle, empty slots are normally removed, so they may be absent.
/// Entries are sorted by slot key.
/// Use `..` when matching this struct because more fields may be added.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ControllerSnapshot {
    /// Slot views captured by this snapshot, sorted by slot key.
    pub slots: Vec<SlotView>,
}

impl ControllerSnapshot {
    /// Number of entries in this snapshot whose status is `Running`.
    ///
    /// This counts owners whose registration was accepted and whose terminal completion has not yet been applied by the controller.
    /// It does not count task bodies executing at this exact moment, or `Admitting` and `Terminating` slots.
    #[must_use]
    pub fn running_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.status == SlotStatusKind::Running)
            .count()
    }

    /// Total pending queue depth across entries in this snapshot.
    ///
    /// This includes replacement heads and excludes current slot owners.
    #[must_use]
    pub fn total_queued(&self) -> usize {
        self.slots.iter().map(|s| s.queue_depth).sum()
    }

    /// Returns the view for one slot if it is present in this snapshot.
    ///
    /// `name` is the slot key, not necessarily the task name.
    #[must_use]
    pub fn slot(&self, name: &str) -> Option<&SlotView> {
        self.slots.iter().find(|s| &*s.slot == name)
    }

    /// Number of slot entries in this snapshot.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Returns `true` when this snapshot contains no slot entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}
