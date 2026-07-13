//! Task submission and run identity.
//!
//! [`TaskId`] is the opaque identity reserved for one task submission.
//!
//! Direct `add*` APIs return it after the registry accepts the task. Controller
//! `submit*` APIs return it after queueing, before slot admission, so the same
//! identity also correlates a submission that is later rejected without running.
//! Users can read it, compare it, log it, and pass it back to runtime APIs such as cancel/remove.
//! Users cannot construct arbitrary ids.
//!
//! ## TaskId vs Name vs Slot
//!
//! | Concept          | Owned by   | Meaning                                                     |
//! |------------------|------------|-------------------------------------------------------------|
//! | [`TaskId`]       | Taskvisor | unique identity of one submission and its run, if admitted |
//! | task name        | task       | human label used for logs, metrics, and registry uniqueness |
//! | controller slot  | controller | admission key for "one at a time" scheduling                |
//!
//! A task name may be reused after the previous task is removed.
//! A `TaskId` is never reused.
//!
//! With the `controller` feature, a submitted task also has a slot key (`ControllerSpec::slot_name`).
//! The slot controls admission only; it is not the same thing as the submission identity.

use std::sync::atomic::{AtomicU64, Ordering};

/// Process-global monotonic source of task identities.
///
/// Starts at `1`; `0` is never minted.
static TASK_ID_SEQ: AtomicU64 = AtomicU64::new(1);

/// Opaque identity of one task submission and its run, if admitted.
///
/// A `TaskId` is minted by Taskvisor and carried on submission and lifecycle
/// [`Event`](crate::Event)s. A controller submission keeps the same id from
/// queueing through admission, execution, and terminal cleanup. Rejected work
/// keeps its id even though no task body ran.
///
/// Use it to:
/// - correlate events for the same submission and run,
/// - cancel or remove a task by identity,
/// - keep stable external logs/metrics references.
///
/// Do not parse display output.
/// Use [`get`](Self::get) when a numeric value is needed for logs or external correlation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(u64);

impl TaskId {
    /// Mints the next submission/run identity.
    ///
    /// Internal only: Taskvisor owns identity allocation across direct runtime
    /// registration and controller pre-admission.
    #[inline]
    pub(crate) fn next() -> Self {
        TaskId(TASK_ID_SEQ.fetch_add(1, Ordering::Relaxed))
    }

    /// Returns the raw numeric value.
    ///
    /// This is intended for logs, metrics, and external correlation.
    /// It does not allow reconstructing a `TaskId`.
    #[inline]
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }

    /// Creates a fresh id for tests.
    ///
    /// ```rust
    /// use taskvisor::TaskId;
    ///
    /// let a = TaskId::for_tests();
    /// let b = TaskId::for_tests();
    /// assert_ne!(a, b);
    /// ```
    #[cfg(feature = "test-util")]
    #[cfg_attr(docsrs, doc(cfg(feature = "test-util")))]
    #[must_use]
    pub fn for_tests() -> Self {
        Self::next()
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

    #[cfg(feature = "test-util")]
    #[test]
    fn for_tests_draws_from_the_runtime_sequence() {
        let runtime = TaskId::next();
        let test = TaskId::for_tests();
        let runtime_after = TaskId::next();

        assert!(test.get() > runtime.get(), "test ids share the sequence");
        assert!(runtime_after.get() > test.get(), "no collision is possible");
    }
}
