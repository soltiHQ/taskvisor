//! Runtime task identity.
//!
//! [`TaskId`] is the opaque runtime identity of one task run.
//!
//! It is minted by Taskvisor when a task is accepted into the runtime.
//! Users can read it, compare it, log it, and pass it back to runtime APIs such as cancel/remove.
//! Users cannot construct arbitrary ids.
//!
//! ## TaskId vs Name vs Slot
//!
//! | Concept          | Owned by   | Meaning                                                     |
//! |------------------|------------|-------------------------------------------------------------|
//! | [`TaskId`]       | runtime    | unique identity of one task run                             |
//! | task name        | task       | human label used for logs, metrics, and registry uniqueness |
//! | controller slot  | controller | admission key for "one at a time" scheduling                |
//!
//! A task name may be reused after the previous task is removed.
//! A `TaskId` is never reused.
//!
//! With the `controller` feature, a submitted task also has a slot key (`ControllerSpec::slot_name`).
//! The slot controls admission only; it is not the same thing as the task's runtime identity.

use std::sync::atomic::{AtomicU64, Ordering};

/// Process-global monotonic source of task identities.
///
/// Starts at `1`; `0` is never minted.
static TASK_ID_SEQ: AtomicU64 = AtomicU64::new(1);

/// Opaque runtime identity of one task run.
///
/// A `TaskId` is minted by the runtime and carried on lifecycle [`Event`](crate::Event)s.
///
/// Use it to:
/// - correlate events for the same run,
/// - cancel or remove a task by identity,
/// - keep stable external logs/metrics references.
///
/// Do not parse display output.
/// Use [`get`](Self::get) when a numeric value is needed for logs or external correlation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(u64);

impl TaskId {
    /// Mints the next runtime identity.
    ///
    /// Internal only: task identities are owned by the runtime.
    #[inline]
    pub(crate) fn next() -> Self {
        TaskId(TASK_ID_SEQ.fetch_add(1, Ordering::Relaxed))
    }

    /// Returns the raw numeric value.
    ///
    /// This is intended for logs, metrics, and external correlation.
    /// It does not allow reconstructing a `TaskId`.
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