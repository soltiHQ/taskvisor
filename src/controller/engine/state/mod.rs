//! Stores controller admission state.
//!
//! This module tree stores mutable admission data indexed by slot and [`TaskId`].
//! [`ControllerState`] holds cross-slot indexes. [`SlotState`] holds the owner
//! phase and pending queue for one slot.
//!
//! ```text
//! ControllerState
//!      ├── slot name ──► shared SlotState
//!      ├── queued TaskId ──► slot name
//!      ├── capacity TaskId ──► retained admission
//!      └── watched TaskId ──► outcome sender
//! ```
//!
//! The global state lock protects the indexes. Each slot has its own async lock
//! for phase and queue changes. The serialized controller loop keeps queue and
//! reverse-index mutations in one transition.
//!
//! The `slot` module defines the slot state machine. Admission and completion
//! handlers drive it with registry results and physical completion signals.

use std::{collections::HashMap, sync::Arc};

use tokio::sync::Mutex;

use crate::{core::OutcomeTx, identity::TaskId};

mod slot;

pub(in crate::controller::engine) use slot::{
    AdmissionTransition, PendingSubmission, ReplaceAction, SlotPhase, SlotState,
};

/// Admission retained while its slot waits for registry command capacity.
pub(in crate::controller::engine) struct CapacityPending {
    /// Slot whose owner is waiting for registry capacity.
    pub(in crate::controller::engine) slot_name: Arc<str>,
    /// Task payload not yet committed to the registry.
    pub(in crate::controller::engine) pending: PendingSubmission,
}

/// Cross-slot indexes owned by the controller engine.
#[derive(Default)]
pub(in crate::controller::engine) struct ControllerState {
    /// Slot state indexed by slot name.
    pub(in crate::controller::engine) slots: HashMap<Arc<str>, Arc<Mutex<SlotState>>>,
    /// Reverse lookup for submissions stored in slot queues.
    pub(in crate::controller::engine) queued_slots: HashMap<TaskId, Arc<str>>,
    /// Retained admissions indexed while registry command capacity is pending.
    pub(in crate::controller::engine) capacity_pending: HashMap<TaskId, CapacityPending>,
    /// Watched outcome senders retained while the controller owns the submission.
    pub(in crate::controller::engine) watchers: HashMap<TaskId, OutcomeTx>,
}

impl ControllerState {
    /// Returns the submissions charged to the aggregate pending-work limit.
    pub(in crate::controller::engine) fn pending_len(&self) -> usize {
        self.queued_slots.len() + self.capacity_pending.len()
    }
}
