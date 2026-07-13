//! # Read-only controller state
//!
//! This module exposes the public snapshot types returned by [`SupervisorHandle::controller_snapshot`](crate::SupervisorHandle::controller_snapshot).
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
    /// A submission was admitted and sent to the runtime, but registration is not confirmed yet.
    ///
    /// The slot is waiting for the direct registry Add reply.
    Admitting,
    /// The registry accepted the task, and it owns the slot.
    ///
    /// This does not prove that the task body is executing now.
    /// It may be waiting for the supervisor's global concurrency permit.
    Running,
    /// The current slot owner is being retired.
    ///
    /// The controller is either waiting for an in-flight Add decision before it can order removal,
    /// or waiting for reliable terminal registry cleanup after removal was ordered.
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
    /// In `Admitting`, this identity has been allocated for the controller submission but the runtime registry has not accepted it yet.
    /// The same can be true in `Terminating` when `Replace` arrived during admission.
    pub owner_id: Option<TaskId>,

    /// Number of pending submissions in this slot's queue.
    ///
    /// This does not include the current slot owner.
    pub queue_depth: usize,

    /// How long the slot has been in its current status.
    ///
    /// `Idle` reports `Duration::ZERO`.
    pub status_for: Duration,
}

/// Point-in-time snapshot of tracked controller slots.
///
/// This is a read-side view for observability.
/// It is not a lifecycle guarantee.
///
/// The snapshot is not globally atomic.
/// Taskvisor reads slots one by one; busy controller can change between two entries.
/// Treat all values as gauges, not as a transactional view.
///
/// Only currently tracked slots are included.
/// A slot that becomes idle and empty may be removed and therefore absent.
/// Entries are sorted by slot key.
/// Use `..` when matching this struct because more fields may be added.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ControllerSnapshot {
    /// Currently tracked slots, sorted by slot key.
    pub slots: Vec<SlotView>,
}

impl ControllerSnapshot {
    /// Number of slots currently in `Running`.
    ///
    /// This counts registered slot owners, not task bodies executing at this exact moment.
    /// It does not count `Admitting` or `Terminating` slots.
    #[must_use]
    pub fn running_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.status == SlotStatusKind::Running)
            .count()
    }

    /// Total number of queued submissions across all tracked slots.
    ///
    /// This does not include current slot owners.
    #[must_use]
    pub fn total_queued(&self) -> usize {
        self.slots.iter().map(|s| s.queue_depth).sum()
    }

    /// Returns the view for one slot, if that slot is currently tracked.
    ///
    /// `name` is the slot key, not necessarily the task name.
    #[must_use]
    pub fn slot(&self, name: &str) -> Option<&SlotView> {
        self.slots.iter().find(|s| &*s.slot == name)
    }

    /// Number of currently tracked slots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Returns `true` when no slots are currently tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}
