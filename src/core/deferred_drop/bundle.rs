//! Carries accepted user-owned values to isolated cleanup.
//!
//! A [`DropReservation`] is bound to one retained value before controller, registry, or subscriber ownership begins.
//! Those paths may add an undelivered outcome, a caught panic payload, or a physical attempt result to [`DropBundle`].
//! Submission turns the collected values into one worker batch.
//!
//! A bundle has one required retained value and at most one outcome, one auxiliary value, and one panic reporter.
//! Its [`Drop`] implementation submits defensively when a normal terminal path does not.

use std::{
    any::Any,
    sync::{Arc, Mutex},
};

use crate::{core::outcome::TaskOutcome, tasks::TaskRef};

use super::{
    capacity::OwnershipPermit,
    executor::{DropBatch, DropExecutor},
};

/// Type-erased destructor job for one retained terminal value.
pub(super) type DropJob = Box<dyn FnOnce() + Send + 'static>;

/// One-shot diagnostic callback used after the first destructor panic.
pub(super) type PanicReporter = Box<dyn FnOnce(String) + Send + 'static>;

/// Capacity held before Taskvisor accepts one user lifetime.
pub(crate) struct DropReservation {
    /// Keeps the cleanup executor available while this reservation exists.
    pub(super) executor: Arc<DropExecutor>,
    /// Capacity unit held for the pending ownership hand-off.
    pub(super) permit: Option<OwnershipPermit>,
}

impl DropReservation {
    /// Connects one charged permit to its started executor.
    pub(super) fn new(executor: Arc<DropExecutor>, permit: OwnershipPermit) -> Self {
        Self {
            executor,
            permit: Some(permit),
        }
    }

    /// Binds this reservation to the value retained across the hand-off.
    pub(crate) fn bundle<T>(self, retained: T) -> DropBundle
    where
        T: Send + 'static,
    {
        DropBundle::new(self, Box::new(move || drop(retained)))
    }

    /// Consumes the permit and submits one complete worker batch.
    fn submit(
        mut self,
        retained: DropJob,
        undelivered_outcome: Option<DropJob>,
        auxiliary: Option<DropJob>,
        panic_reporter: Option<PanicReporter>,
        poisoned: bool,
    ) {
        let permit = self
            .permit
            .take()
            .expect("one ownership reservation submits at most one bundle");
        self.executor.submit(DropBatch::new(
            permit,
            retained,
            undelivered_outcome,
            auxiliary,
            panic_reporter,
            poisoned,
        ));
    }
}

/// Terminal values collected before one worker submission.
struct DropBundleInner {
    /// Charged ownership transferred with this bundle.
    reservation: DropReservation,
    /// Value whose lifetime this reservation protects.
    retained: DropJob,
    /// Final outcome that no watcher accepted.
    undelivered_outcome: Option<DropJob>,
    /// Caught panic payload or completed physical attempt result.
    auxiliary: Option<DropJob>,
    /// Diagnostic callback available to the first failing destructor.
    panic_reporter: Option<PanicReporter>,
    /// Whether clean destructor returns must still retire the permit.
    poisoned: bool,
}

/// Mutable terminal state for one charged user lifetime.
///
/// The mutex lets explicit and defensive submission take the contents once.
/// User destructors never run while this mutex is held.
pub(crate) struct DropBundle {
    /// Bundle contents until the first explicit or defensive submission.
    inner: Mutex<Option<DropBundleInner>>,
}

impl DropBundle {
    /// Initializes the required to be retained job with all optional slots empty.
    fn new(reservation: DropReservation, retained: DropJob) -> Self {
        Self {
            inner: Mutex::new(Some(DropBundleInner {
                reservation,
                retained,
                undelivered_outcome: None,
                auxiliary: None,
                panic_reporter: None,
                poisoned: false,
            })),
        }
    }

    /// Provides unsubmitted contents for terminal attachments.
    fn inner_mut(&mut self) -> Option<&mut DropBundleInner> {
        self.inner
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .as_mut()
    }

    /// Stores a final outcome that its watcher did not accept.
    ///
    /// A duplicate poisons the bundle. An outcome received after storage is unavailable is retained permanently.
    /// Both paths avoid caller-context destruction.
    pub(crate) fn attach_outcome(&mut self, outcome: TaskOutcome) {
        let Some(inner) = self.inner_mut() else {
            std::mem::forget(outcome);
            return;
        };
        if inner.undelivered_outcome.is_some() {
            inner.poisoned = true;
            std::mem::forget(outcome);
            return;
        }
        inner.undelivered_outcome = Some(Box::new(move || drop(outcome)));
    }

    /// Uses the auxiliary slot to retain a caught panic payload.
    pub(crate) fn attach_panic_payload(&mut self, payload: Box<dyn Any + Send>) {
        self.attach_auxiliary(payload);
    }

    /// Sets one diagnostic callback for the first destructor panic.
    ///
    /// A second reporter is retained permanently and poisons the bundle.
    pub(crate) fn set_panic_reporter<F>(&mut self, reporter: F)
    where
        F: FnOnce(String) + Send + 'static,
    {
        let reporter: PanicReporter = Box::new(reporter);
        let Some(inner) = self.inner_mut() else {
            std::mem::forget(reporter);
            return;
        };
        if inner.panic_reporter.is_some() {
            inner.poisoned = true;
            std::mem::forget(reporter);
            return;
        }
        inner.panic_reporter = Some(reporter);
    }

    /// Uses the auxiliary slot after logical terminal commit to retain the physical result.
    pub(crate) fn attach_physical<T>(&mut self, value: T)
    where
        T: Send + 'static,
    {
        self.attach_auxiliary(value);
    }

    /// Stores the single auxiliary value accepted by this bundle.
    ///
    /// A duplicate poisons the bundle.
    /// A value received after storage is unavailable is retained permanently.
    fn attach_auxiliary<T>(&mut self, value: T)
    where
        T: Send + 'static,
    {
        let Some(inner) = self.inner_mut() else {
            std::mem::forget(value);
            return;
        };
        if inner.auxiliary.is_some() {
            inner.poisoned = true;
            std::mem::forget(value);
            return;
        }
        inner.auxiliary = Some(Box::new(move || drop(value)));
    }

    /// Marks the charged unit for retirement after worker cleanup.
    pub(crate) fn poison(&mut self) {
        if let Some(inner) = self.inner_mut() {
            inner.poisoned = true;
        }
    }

    /// Consumes the bundle and schedules all collected jobs.
    pub(crate) fn submit(self) {
        self.submit_inner();
    }

    /// Takes the contents once before entering the executor queue.
    fn submit_inner(&self) {
        let Some(inner) = self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        else {
            return;
        };
        inner.reservation.submit(
            inner.retained,
            inner.undelivered_outcome,
            inner.auxiliary,
            inner.panic_reporter,
            inner.poisoned,
        );
    }
}

impl Drop for DropBundle {
    /// Uses the same one-shot path when normal terminal code releases the bundle.
    fn drop(&mut self) {
        self.submit_inner();
    }
}

/// Moves task data together with its reserved cleanup ownership.
///
/// `value` is declared first.
/// Defensive field destruction releases its task reference while `cleanup` still retains the final reference.
pub(crate) struct OwnedTask<T> {
    /// Task or controller data carried through the internal hand-off.
    pub(crate) value: T,
    /// Charged bundle retaining the final user task reference.
    pub(crate) cleanup: DropBundle,
}

impl<T> OwnedTask<T> {
    /// Establishes cleanup ownership before the task data is handed off.
    pub(crate) fn new(value: T, retained: TaskRef, reservation: DropReservation) -> Self {
        Self {
            value,
            cleanup: reservation.bundle(retained),
        }
    }

    /// Transforms the carried task data without changing its cleanup lifetime.
    #[cfg(feature = "controller")]
    pub(crate) fn map<U>(self, map: impl FnOnce(T) -> U) -> OwnedTask<U> {
        let Self { value, cleanup } = self;
        OwnedTask {
            value: map(value),
            cleanup,
        }
    }

    /// Transfers task data and cleanup ownership to their next internal owners.
    pub(crate) fn into_parts(self) -> (T, DropBundle) {
        let Self { value, cleanup } = self;
        (value, cleanup)
    }
}
