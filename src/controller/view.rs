//! Read-only controller slot views.
//!
//! This module exposes the public snapshot types returned by [`SupervisorHandle::controller_snapshot`](crate::SupervisorHandle::controller_snapshot).
//!
//! Use these views for dashboards, metrics, health checks, and tests.
//! They are pull-based introspection, so callers do not need to parse controller events.

use std::sync::Arc;
use std::time::Duration;

use crate::identity::TaskId;

/// Public status of a controller slot.
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
    /// The registry accepted the task, and the slot is occupied.
    Running,
    /// The current slot owner is being retired.
    ///
    /// The controller is waiting for the runtime to publish `TaskRemoved`.
    Terminating,
}

/// Point-in-time view of one controller slot.
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

    /// Current slot owner, if any.
    ///
    /// Present for `Admitting`, `Running`, and `Terminating`.
    /// Absent for `Idle`.
    pub running: Option<TaskId>,

    /// Number of pending submissions in this slot's queue.
    ///
    /// This does not include the current slot owner.
    pub queue_depth: usize,

    /// How long the slot has been in its current status.
    ///
    /// `Idle` reports `Duration::ZERO`.
    pub status_for: Duration,
}

/// Point-in-time snapshot of controller slots.
///
/// This is a read-side view for observability.
/// It is not a lifecycle guarantee.
///
/// The snapshot is not globally atomic: slots are sampled one by one.
/// Treat all counts and durations as gauges.
///
/// Only currently tracked slots are included.
/// A slot that became idle and empty may be garbage-collected and therefore absent from the snapshot.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ControllerSnapshot {
    /// Currently tracked slots, sorted by slot key.
    pub slots: Vec<SlotView>,
}

impl ControllerSnapshot {
    /// Number of slots currently in `Running`.
    ///
    /// This does not count `Admitting` or `Terminating` slots.
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
