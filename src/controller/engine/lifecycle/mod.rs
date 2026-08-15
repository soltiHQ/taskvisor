//! Controller loop lifecycle and shutdown.
//!
//! One task owns the ordered command receiver and polls all tracked operations.
//! It applies commands and authoritative runtime results to controller state.
//!
//! ```text
//! command intake  ──► controller loop ──► state transitions
//! runtime results ──► controller loop
//! shutdown signal ──► controller loop
//! ```
//!
//! Shutdown closes intake, drains accepted commands, resolves pending replies,
//! and clears controller-owned state before the task reports completion.

mod driver;
mod shutdown;
mod task;

pub(super) use task::ControllerTask;
