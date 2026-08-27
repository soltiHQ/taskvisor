//! Provides an operational view of current controller slots.
//!
//! [`SupervisorHandle::controller_snapshot`](crate::SupervisorHandle::controller_snapshot) reads
//! slot owners, states, queue depths, and state ages directly from the admission engine.
//! Use the result for status pages, metrics collection, diagnostics, and tests.
//! It does not control admission or consume events.
//!
//! ```text
//! controller admission engine
//!          │ tracked slot state
//!          ▼
//! SupervisorHandle::controller_snapshot
//!          │ reads slots one at a time
//!          ▼
//! ControllerSnapshot ──► status, metrics, and diagnostics
//! ```
//!
//! Each slot is internally consistent.
//! The full collection is a rolling view because slots are read one at a time.
//! It is not an atomic snapshot of the whole controller.
//! Use [`TaskWaiter`](crate::TaskWaiter) for a reliable final result of one watched submission.

use std::sync::Arc;
use std::time::Duration;

use crate::identity::TaskId;

/// The admission and ownership state of a controller slot.
///
/// This status describes admission and ownership.
/// It does not always describe what the task body is doing at that moment.
///
/// This enum is non-exhaustive.
/// Use a wildcard arm when matching it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SlotStatusKind {
    /// The slot had no owner when it was read.
    Idle,

    /// The controller has not applied the runtime registry admission decision for the owner.
    ///
    /// The controller may still be waiting for runtime command capacity.
    /// It may instead be waiting to receive or apply the registration result.
    Admitting,

    /// The runtime registry accepted the owner with no replacement retirement pending.
    ///
    /// The task body may be waiting, sleeping between attempts, or finishing cleanup.
    /// A later `Replace` request changes this status to `Terminating`.
    /// The current owner remains until physical release.
    Running,

    /// The current owner is being retired after a replacement request.
    ///
    /// The controller may still be waiting for the owner's registration result.
    /// It may instead be waiting for registry cleanup or physical release.
    /// The replacement that triggered this state may already have been removed from the queue.
    Terminating,
}

/// One controller slot captured at a single point during collection.
///
/// All fields come from one locked slot state.
/// Another slot may be read at a different time.
///
/// This struct is non-exhaustive.
/// Use `..` when matching it.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SlotView {
    /// Effective admission key used to group non-overlapping work.
    pub slot: Arc<str>,

    /// Status captured for this slot.
    pub status: SlotStatusKind,

    /// Task ID that owns the captured slot state.
    ///
    /// This is `None` for [`SlotStatusKind::Idle`].
    /// During [`SlotStatusKind::Admitting`], the controller has not applied a successful registry decision yet.
    pub owner_id: Option<TaskId>,

    /// Number of submissions waiting behind the current owner.
    ///
    /// This includes a replacement waiting at the front of the queue and excludes the owner.
    pub queue_depth: usize,

    /// Time elapsed since the captured status began.
    ///
    /// This measures slot state time, not task execution time.
    /// `Idle` reports `Duration::ZERO`.
    pub status_for: Duration,
}

/// A rolling read-only view of the slots tracked by one controller.
///
/// Each [`SlotView`] reports one slot's owner, status, pending depth, and time in that status.
/// Entries appear in slot-key order.
///
/// # Consistency
///
/// The controller does not read every slot at one exact moment. A slot created during collection may be absent.
/// A removed slot may still appear. Any state can change after this value is returned. Buffered controller commands do not appear.
/// A command can affect this view only after the controller processes it.
///
/// This struct is non-exhaustive.
/// Use `..` when matching it.
///
/// # Examples
///
/// ```rust,no_run
/// # use taskvisor::prelude::*;
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # let supervisor = Supervisor::builder(SupervisorConfig::default())
/// #     .with_controller(ControllerConfig::default())
/// #     .build();
/// # let handle = supervisor.serve()?;
/// if let Some(snapshot) = handle.controller_snapshot().await {
///     println!(
///         "{} running, {} queued",
///         snapshot.running_count(),
///         snapshot.total_queued(),
///     );
///
///     if let Some(deploy) = snapshot.slot("deploy") {
///         println!("deploy: {:?}, queued: {}", deploy.status, deploy.queue_depth);
///     }
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ControllerSnapshot {
    /// Captured slots in slot-key order.
    pub slots: Vec<SlotView>,
}

impl ControllerSnapshot {
    /// Captured slots in the exact [`SlotStatusKind::Running`] state.
    ///
    /// This excludes `Admitting` and `Terminating`.
    /// Runtime-accepted owners already being retired are also excluded.
    /// It is not a count of task bodies polling at this exact moment.
    #[must_use]
    pub fn running_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.status == SlotStatusKind::Running)
            .count()
    }

    /// Pending submissions across all captured slot queues.
    ///
    /// This includes replacements waiting at the front of a queue and excludes current owners.
    #[must_use]
    pub fn total_queued(&self) -> usize {
        self.slots.iter().map(|s| s.queue_depth).sum()
    }

    /// Captured slot for an exact effective admission key.
    #[must_use]
    pub fn slot(&self, name: &str) -> Option<&SlotView> {
        self.slots.iter().find(|s| &*s.slot == name)
    }

    /// Number of captured slots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether no slots were captured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}
