//! # Per-task admission policy
//!
//! The controller admits **one task per slot**.
//! The slot is the spec's slot key ([`TaskSpec::slot`](crate::TaskSpec::slot)), which falls back to the task
//! name when not set explicitly - so the admission unit is the *slot*, not the task name (several differently-named runs may share one slot).
//!
//! At any given time **one** task may run in a slot.
//! When a new request for the same slot arrives, the admission policy decides what to do.
//!
//! ## Variants
//!
//! - `DropIfRunning`: If the slot is already running, **ignore** the new request.
//! - `Replace`: **Stop** the running task (cancel/remove) and start the new one.
//! - `Queue`: **Enqueue** the new request (FIFO).
//!
//! ## Invariants
//!
//! - Tasks within the same slot never run in parallel (use distinct slots if you need parallel execution).
//! - Queued requests are executed strictly in submission order.

/// Policy controlling how new submissions are handled when a slot is busy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionPolicy {
    /// Skip task if already running.
    ///
    /// Use when:
    /// - You only care about the latest state
    /// - Redundant work should be avoided
    /// - Example: periodic health checks
    DropIfRunning,

    /// Stop current task and start new one immediately.
    ///
    /// Use when:
    /// - New request invalidates old one
    /// - Priority to latest submission
    /// - Example: deployment pipeline (new commit cancels old build)
    Replace,

    /// Queue the task (FIFO order).
    ///
    /// Use when:
    /// - All submissions must execute
    /// - Order matters
    /// - Example: sequential processing pipeline
    Queue,
}
