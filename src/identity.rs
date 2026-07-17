//! # Task identity
//!
//! [`TaskId`] is the opaque identity reserved for one task submission.
//!
//! Direct `add*` methods return it after the registry accepts a task.
//! Controller `submit*` methods return it after queueing, before slot admission.
//! Controller `prepare_submission` exposes it earlier, before the submission can
//! publish any event.
//! The same ID therefore also identifies a controller submission that is rejected without running.
//!
//! ## One identity across the lifecycle
//!
//! Every submission accepted into Taskvisor's command path has one `TaskId`.
//! It is allocated before the first admission decision, then passed unchanged through every stage that the submission reaches:
//!
//! ```text
//! allocate TaskId A
//!         │
//!         ├── direct add ───────────────────────────────────────────────┐
//!         │                                                             │
//!         └── optional controller ─► queue[A] ─► slot admission[A] ─────┤
//!                                      │                                │
//!                                      └──► rejected[A] (not registered)│
//!                                                                       ▼
//!                                                         registry admission[A]
//!                                                             │        │
//!                                      rejected[A] ◄──────────┘        └──► registry[A]
//!                                      (not registered)                     │
//!                                                                            ▼
//!                                              task runner[A] ─► attempt 1, 2, ...
//!                                                                            ▼
//!                                                                       cleanup[A]
//! ```
//!
//! No new `TaskId` is allocated at admission or between retry attempts.
//! Queue management, registry membership, the managed runner, and completion tracking carry the same `A`.
//! Related lifecycle events expose it so callers can correlate cancellation, logs, and metrics.
//!
//! ## TaskId vs Name vs Slot
//!
//! | Concept          | Owned by   | Meaning                                               |
//! |------------------|------------|-------------------------------------------------------|
//! | [`TaskId`]       | taskvisor  | identity of one submission across its full lifecycle  |
//! | task name        | task       | label used for logs, metrics, and registry uniqueness |
//! | controller slot  | controller | admission key for "one at a time" scheduling          |
//!
//! A task name is unique only while its registry entry exists.
//! After terminal cleanup removes that entry, a later submission may reuse the same name.
//! Reusing a name does not reuse its identity:
//!
//! ```text
//! first submission:  name = "worker", slot = "jobs", TaskId = A
//! terminal cleanup:  removes A and releases the name "worker"
//! later submission:  name = "worker", slot = "jobs", TaskId = B (B != A)
//! ```
//!
//! Each `TaskId` is allocated from a process-local `u64` counter.
//!
//! The counter is not stored across process restarts, and a `TaskId` is not a UUID.
//! If external systems need a persistent identity, store their own ID next to this one.
//!
//! With the `controller` feature, a submitted task also has a slot key (`ControllerSpec::slot_name`).
//! The slot controls admission only; it is not the same thing as the submission identity.

use std::sync::atomic::{AtomicU64, Ordering};

/// Process-local allocation counter for task identities.
///
/// Starts at `1`.
/// Like all `AtomicU64::fetch_add` counters, it wraps after `2^64` allocations;
/// reaching that limit is outside normal operation.
static TASK_ID_SEQ: AtomicU64 = AtomicU64::new(1);

/// Opaque process-local identity of one task submission.
///
/// Taskvisor allocates it once. With the `controller` feature, this happens before admission so queued work can already be addressed and correlated.
/// A prepared controller submission exposes the allocated value before controller intake and before any event for that value can be published.
/// If admitted, the same value becomes the registry key and remains unchanged through all attempts and terminal cleanup.
///
/// Rejected work keeps its id even though no task body ran.
///
/// Pass the returned value to cancellation and removal operations.
/// The same value correlates the submission's lifecycle [`Event`](crate::Event)s, completion, logs, and metrics within the current process.
///
/// Do not parse its display output.
/// Use [`get`](Self::get) when you need the numeric value.
/// Numeric order is allocation order, not a causal or time order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(u64);

impl TaskId {
    /// Allocates the next submission identity.
    ///
    /// Internal only: Taskvisor owns identity allocation across direct runtime registration and controller pre-admission.
    #[inline]
    pub(crate) fn next() -> Self {
        TaskId(TASK_ID_SEQ.fetch_add(1, Ordering::Relaxed))
    }

    /// Returns the process-local numeric value.
    ///
    /// This is useful for logs, metrics, and correlation within one process run.
    /// The value is not persistent across restarts and cannot reconstruct a `TaskId` through the public API.
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
    fn ids_are_nonzero_unique_and_monotonic() {
        let a = TaskId::next();
        let b = TaskId::next();

        assert!(a.get() >= 1, "zero is reserved and must never be minted");
        assert!(b.get() > a.get(), "ids must increase: {a} then {b}");
        assert_ne!(a, b);
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
