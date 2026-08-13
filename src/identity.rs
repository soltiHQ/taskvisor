//! Explains the identities used to submit, manage, and coordinate tasks.
//!
//! | Value           | Meaning                                      |
//! |-----------------|----------------------------------------------|
//! | [`TaskId`]      | One submission in the current process        |
//! | Task name       | Registry uniqueness key and diagnostic label |
//! | Controller slot | Key that coordinates competing submissions   |
//!
//! ```text
//! application submission
//!      ├── direct add ─────────► TaskId + task name ──► registry
//!      └── controller submit ──► TaskId + slot ───────► controller ──► registry
//! ```
//!
//! Taskvisor allocates the ID before the first admission decision. The same ID follows queued work,
//! every retry, terminal cleanup, and controller rejection. Several task names may use the same controller slot.
//!
//! A name can be reused after registry membership ends and Taskvisor has observed the physical
//! exit of any force-aborted actor with that name. Reuse allocates a new [`TaskId`].
//! IDs come from a process-local `u64` sequence, are not persisted, and cannot be reconstructed
//! through the public API. Returned IDs are never zero and never wrap. The next allocation after
//! exhaustion panics. Store a separate application ID when identity must survive a process restart.

use std::sync::atomic::{AtomicU64, Ordering};

/// Process-local sequence used for every task identity.
///
/// Starts at `1`; zero is the exhausted sentinel and is never returned.
static TASK_ID_SEQ: AtomicU64 = AtomicU64::new(1);

#[inline]
fn advance(current: u64) -> Option<u64> {
    match current {
        0 => None,
        u64::MAX => Some(0),
        value => Some(value + 1),
    }
}

/// Opaque identity of one task submission within the current process.
///
/// Use it for cancellation, removal, watched outcomes, and event correlation.
/// An admitted task keeps the same value through every attempt and terminal cleanup.
/// Controller rejection also keeps the submitted value even though no task body runs.
///
/// `Display` writes `#` followed by the numeric value. Do not parse that output;
/// use [`get`](Self::get). Numeric order records allocation order only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(u64);

impl TaskId {
    /// Allocates the next submission identity.
    ///
    /// Taskvisor owns the single sequence used by direct, static-run, and controller paths.
    #[inline]
    pub(crate) fn next() -> Self {
        let id = TASK_ID_SEQ
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, advance)
            .unwrap_or_else(|_| panic!("TaskId space exhausted; identities cannot wrap safely"));
        TaskId(id)
    }

    /// Returns the process-local numeric value.
    ///
    /// The number is useful for logs and metrics in the current process.
    /// It is not persistent and cannot reconstruct a `TaskId` through the public API.
    #[inline]
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }

    /// Creates a fresh ID for tests.
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
    fn ids_are_nonzero_unique_and_monotonic() {
        let a = TaskId::next();
        let b = TaskId::next();

        assert!(a.get() >= 1, "zero is reserved and must never be minted");
        assert!(b.get() > a.get(), "ids must increase: {a} then {b}");
        assert_ne!(a, b);
    }

    #[test]
    fn sequence_uses_zero_as_an_exhausted_sentinel() {
        assert_eq!(advance(1), Some(2));
        assert_eq!(advance(u64::MAX - 1), Some(u64::MAX));
        assert_eq!(advance(u64::MAX), Some(0));
        assert_eq!(advance(0), None);
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
