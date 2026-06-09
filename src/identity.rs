//! # Runtime task identity.
//!
//! [`TaskId`] is the **runtime identity** of a single task run instance, minted by the supervisor when a task is admitted.
//!
//! ## Identity vs. label vs. slot
//!
//! - [`TaskId`] - *identity*: unique per run instance, owned by the runtime (this crate), never reused.
//!   Use it to correlate lifecycle [`Event`](crate::Event)s and to address a task for cancellation/removal.
//! - **name** ([`Task::name`](crate::Task::name)) - *human label*: free-form, **not** required to be unique.
//!   Used for logs, metrics, and external resource naming.
//! - **slot** ([`TaskSpec::slot`](crate::TaskSpec::slot)) - *admission key*: the logical unit the controller admits one-at-a-time.

use std::sync::atomic::{AtomicU64, Ordering};

/// Process-global monotonic source of task identities (starts at 1; 0 is never minted).
static TASK_ID_SEQ: AtomicU64 = AtomicU64::new(1);

/// Unique runtime identity of a single task run instance.
///
/// Minted by the supervisor at admission time and carried on every lifecycle [`Event`](crate::Event) for that run via [`Event::id`](crate::Event::id).
/// Unlike the task **name** (a free-form human label), a `TaskId` is unique and never reused, it unambiguously correlates events even when names repeat or runs overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(u64);

impl TaskId {
    /// Mints the next unique identity. Internal: only the runtime assigns identities.
    #[inline]
    pub(crate) fn next() -> Self {
        TaskId(TASK_ID_SEQ.fetch_add(1, Ordering::Relaxed))
    }

    /// Returns the raw numeric value (for logging / external correlation).
    #[inline]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_monotonic() {
        let a = TaskId::next();
        let b = TaskId::next();
        assert!(b.get() > a.get(), "ids must increase: {a} then {b}");
        assert_ne!(a, b);
    }

    #[test]
    fn never_mints_zero() {
        assert!(TaskId::next().get() >= 1);
    }
}
