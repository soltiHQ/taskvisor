//! Controller loop lifecycle and shutdown.
//!
//! One task owns the ordered command receiver and all tracked operations.
//! Only this task applies command and runtime-result transitions to controller state.
//!
//! ```text
//! command intake  ──► controller loop ──► state transitions
//! runtime results ──► controller loop
//! shutdown signal ──► controller loop
//! ```
//!
//! Shutdown closes intake and drains accepted commands.
//! It resolves pending replies and clears controller-owned state before the task reports completion.

mod driver;
mod shutdown;
mod task;

pub(super) use task::ControllerTask;
