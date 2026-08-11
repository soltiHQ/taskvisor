//! Strictly bounded isolation for library-owned user destructors.
//!
//! A synchronous destructor cannot be interrupted safely. Taskvisor therefore
//! reserves one process-wide ownership slot before a user-owned task or
//! subscriber lifetime crosses an internal boundary. The reservation remains
//! charged until every value in that lifetime's terminal bundle has been
//! destroyed cleanly.
//!
//! Blocking destructors occupy one of a fixed number of worker threads. A
//! destructor panic permanently consumes its reservation: releasing that slot
//! would allow an unlimited stream of new hostile values to leak panic state.

use std::{
    any::Any,
    collections::VecDeque,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU8, Ordering as AtomicOrdering},
        mpsc::{Receiver, Sender, channel},
    },
};

use tokio::sync::Notify;

use crate::{core::outcome::TaskOutcome, tasks::TaskRef};

type DropJob = Box<dyn FnOnce() + Send + 'static>;
type PanicReporter = Box<dyn FnOnce(String) + Send + 'static>;

/// Maximum library-owned user lifetimes across all supervisors in this process.
pub(crate) const OWNERSHIP_CAPACITY: usize = 1024;

/// Stable public resource name for the shared task/subscriber ownership budget.
pub(crate) const OWNERSHIP_RESOURCE: &str = "owned_user_lifetimes";

/// A blocking destructor can occupy at most one of these process-wide workers.
const WORKER_COUNT: usize = 2;

/// Resource-budget admission failure before ownership crosses an internal queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DropCapacityError;

impl DropCapacityError {
    pub(crate) const fn limit(self) -> usize {
        OWNERSHIP_CAPACITY
    }
}

/// One globally charged right to submit a terminal destructor bundle.
pub(crate) struct DropReservation {
    executor: Arc<DropExecutor>,
    permit: Option<OwnershipPermit>,
}

impl DropReservation {
    /// Builds a terminal bundle whose first job retains the user task lifetime.
    pub(crate) fn bundle<T>(self, retained: T) -> DropBundle
    where
        T: Send + 'static,
    {
        DropBundle::new(self, Box::new(move || drop(retained)))
    }

    fn submit(mut self, jobs: Vec<DropJob>, panic_reporter: Option<PanicReporter>, poisoned: bool) {
        let permit = self
            .permit
            .take()
            .expect("one ownership reservation submits at most one bundle");
        self.executor
            .submit(DropBatch::new(permit, jobs, panic_reporter, poisoned));
    }
}

/// One reservation plus the bounded set of terminal values charged to it.
///
/// A task contributes one retained task value and at most one undelivered final
/// outcome. Jobs are kept separate so one panicking destructor cannot make a
/// second user destructor run while the worker is already unwinding.
struct DropBundleInner {
    reservation: DropReservation,
    retained: DropJob,
    undelivered_outcome: Option<DropJob>,
    auxiliary: Option<DropJob>,
    panic_reporter: Option<PanicReporter>,
    poisoned: bool,
}

/// The mutex makes the bundle safe to store inside registry state shared by
/// async tasks. No user destructor runs while it is held: submission first
/// moves the complete inner value out and only then hands it to a worker.
pub(crate) struct DropBundle {
    inner: Mutex<Option<DropBundleInner>>,
}

impl DropBundle {
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

    fn inner_mut(&mut self) -> Option<&mut DropBundleInner> {
        self.inner
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .as_mut()
    }

    /// Retains one final outcome that could not be transferred to its watcher.
    pub(crate) fn attach_outcome(&mut self, outcome: TaskOutcome) {
        let Some(inner) = self.inner_mut() else {
            // Ownership makes this unreachable: submission consumes the
            // bundle. Retaining avoids running user destructors from a
            // defensive cleanup path if that invariant is ever broken.
            std::mem::forget(outcome);
            return;
        };
        if inner.undelivered_outcome.is_some() {
            // A duplicate terminal result is an internal protocol violation.
            // Fail closed without panicking from a finalizer's Drop.
            inner.poisoned = true;
            std::mem::forget(outcome);
            return;
        }
        inner.undelivered_outcome = Some(Box::new(move || drop(outcome)));
    }

    /// Retains a panic payload from synchronous user code under this task's
    /// already charged ownership slot.
    pub(crate) fn attach_panic_payload(&mut self, payload: Box<dyn Any + Send>) {
        self.attach_auxiliary(payload);
    }

    /// Installs the one internal diagnostic callback used if a charged user
    /// destructor panics. The worker forgets the panic payload before invoking
    /// this callback, so a reporting panic cannot trigger a double panic.
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

    /// Retains the physical attempt result transferred from the force-abort
    /// reaper after the logical terminal outcome was already committed.
    pub(crate) fn attach_physical<T>(&mut self, value: T)
    where
        T: Send + 'static,
    {
        self.attach_auxiliary(value);
    }

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

    /// Permanently consumes this reservation after a nested destructor panic
    /// had to retain an unrepresentable panic payload inside the physical
    /// attempt owner.
    pub(crate) fn poison(&mut self) {
        if let Some(inner) = self.inner_mut() {
            inner.poisoned = true;
        }
    }

    /// Transfers the charged bundle to the bounded destructor workers.
    pub(crate) fn submit(self) {
        self.submit_inner();
    }

    fn submit_inner(&self) {
        let Some(inner) = self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        else {
            return;
        };
        let mut jobs = Vec::with_capacity(3);
        jobs.push(inner.retained);
        jobs.extend(inner.undelivered_outcome);
        jobs.extend(inner.auxiliary);
        inner
            .reservation
            .submit(jobs, inner.panic_reporter, inner.poisoned);
    }
}

impl Drop for DropBundle {
    fn drop(&mut self) {
        // Defensive paths still keep the strict reservation. Normal terminal
        // cleanup explicitly routes this bundle through the attempt reaper.
        self.submit_inner();
    }
}

/// A value coupled to the reservation acquired before internal hand-off.
///
/// `value` is declared first deliberately: a defensive ordinary field drop
/// releases its task reference while `cleanup` still retains the final one.
pub(crate) struct OwnedTask<T> {
    pub(crate) value: T,
    pub(crate) cleanup: DropBundle,
}

impl<T> OwnedTask<T> {
    pub(crate) fn new(value: T, retained: TaskRef, reservation: DropReservation) -> Self {
        Self {
            value,
            cleanup: reservation.bundle(retained),
        }
    }

    #[cfg(feature = "controller")]
    pub(crate) fn map<U>(self, map: impl FnOnce(T) -> U) -> OwnedTask<U> {
        let Self { value, cleanup } = self;
        OwnedTask {
            value: map(value),
            cleanup,
        }
    }

    pub(crate) fn into_parts(self) -> (T, DropBundle) {
        let Self { value, cleanup } = self;
        (value, cleanup)
    }
}

/// A worker message keeps its permit outside every user closure.
///
/// If any user destructor panics, the permit is retained forever and the
/// global ownership capacity shrinks. This makes
/// repeated hostile failures fail closed instead of creating repeated leaks.
const CAPACITY_WAITING: u8 = 0;
const CAPACITY_GRANTED: u8 = 1;
const CAPACITY_CLOSED: u8 = 2;
const CAPACITY_TAKEN: u8 = 3;
const CAPACITY_CANCELED: u8 = 4;

struct CapacitySignal {
    status: AtomicU8,
    changed: Notify,
}

impl CapacitySignal {
    fn waiting() -> Self {
        Self {
            status: AtomicU8::new(CAPACITY_WAITING),
            changed: Notify::new(),
        }
    }
}

struct CapacityWaiter {
    units: usize,
    bypassed_units: usize,
    signal: Arc<CapacitySignal>,
}

struct CapacityState {
    available: usize,
    closed: bool,
    waiters: VecDeque<CapacityWaiter>,
}

/// Fair, cancellation-safe ownership admission.
///
/// An older request may be bypassed while it cannot fit, so a large atomic
/// batch does not strand usable slots. Bypass is bounded to one full capacity
/// turnover. Once that budget is consumed, newer requests wait and released
/// capacity accumulates for the older request.
struct CapacityBroker {
    capacity: usize,
    state: Mutex<CapacityState>,
}

enum CapacityStart {
    Ready(OwnershipPermit),
    Waiting(CapacityRequest),
}

struct CapacityRequest {
    broker: Arc<CapacityBroker>,
    units: usize,
    signal: Arc<CapacitySignal>,
    active: bool,
}

impl CapacityRequest {
    async fn wait(mut self) -> Result<OwnershipPermit, DropCapacityError> {
        loop {
            let changed = self.signal.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match self.signal.status.load(AtomicOrdering::Acquire) {
                CAPACITY_WAITING => changed.await,
                CAPACITY_GRANTED => {
                    self.signal
                        .status
                        .store(CAPACITY_TAKEN, AtomicOrdering::Release);
                    self.active = false;
                    return Ok(OwnershipPermit::new(Arc::clone(&self.broker), self.units));
                }
                CAPACITY_CLOSED => {
                    self.active = false;
                    return Err(DropCapacityError);
                }
                _invalid => {
                    self.broker.close();
                    self.active = false;
                    return Err(DropCapacityError);
                }
            }
        }
    }
}

impl Drop for CapacityRequest {
    fn drop(&mut self) {
        if self.active {
            self.broker.cancel(self.units, &self.signal);
        }
    }
}

impl CapacityBroker {
    fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            capacity,
            state: Mutex::new(CapacityState {
                available: capacity,
                closed: false,
                waiters: VecDeque::new(),
            }),
        })
    }

    async fn acquire(self: &Arc<Self>, units: usize) -> Result<OwnershipPermit, DropCapacityError> {
        match self.start_acquire(units)? {
            CapacityStart::Ready(permit) => Ok(permit),
            CapacityStart::Waiting(request) => request.wait().await,
        }
    }

    fn try_acquire(self: &Arc<Self>, units: usize) -> Result<OwnershipPermit, DropCapacityError> {
        if units == 0 || units > self.capacity {
            return Err(DropCapacityError);
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed || !state.waiters.is_empty() || state.available < units {
            return Err(DropCapacityError);
        }
        state.available -= units;
        Ok(OwnershipPermit::new(Arc::clone(self), units))
    }

    fn start_acquire(self: &Arc<Self>, units: usize) -> Result<CapacityStart, DropCapacityError> {
        if units == 0 || units > self.capacity {
            return Err(DropCapacityError);
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed {
            return Err(DropCapacityError);
        }
        if state.waiters.is_empty() && state.available >= units {
            state.available -= units;
            return Ok(CapacityStart::Ready(OwnershipPermit::new(
                Arc::clone(self),
                units,
            )));
        }
        // Requests do not own a user-lifetime permit yet, so bound their
        // internal admission metadata separately. The production broker uses
        // the same fixed limit as physical ownership.
        if state.waiters.len() >= self.capacity {
            return Err(DropCapacityError);
        }

        let signal = Arc::new(CapacitySignal::waiting());
        state.waiters.push_back(CapacityWaiter {
            units,
            bypassed_units: 0,
            signal: Arc::clone(&signal),
        });
        let ready = self.dispatch_locked(&mut state);
        drop(state);
        Self::notify(ready);
        Ok(CapacityStart::Waiting(CapacityRequest {
            broker: Arc::clone(self),
            units,
            signal,
            active: true,
        }))
    }

    fn release(&self, units: usize) {
        if units == 0 {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let ready = if self.return_capacity_locked(&mut state, units) {
            self.dispatch_locked(&mut state)
        } else {
            Self::close_waiters_locked(&mut state)
        };
        drop(state);
        Self::notify(ready);
    }

    fn cancel(&self, units: usize, signal: &Arc<CapacitySignal>) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match signal.status.load(AtomicOrdering::Acquire) {
            CAPACITY_WAITING => {
                if let Some(position) = state
                    .waiters
                    .iter()
                    .position(|waiter| Arc::ptr_eq(&waiter.signal, signal))
                {
                    state.waiters.remove(position);
                }
            }
            CAPACITY_GRANTED => {
                if !self.return_capacity_locked(&mut state, units) {
                    signal
                        .status
                        .store(CAPACITY_CANCELED, AtomicOrdering::Release);
                    let closed = Self::close_waiters_locked(&mut state);
                    drop(state);
                    Self::notify(closed);
                    return;
                }
            }
            CAPACITY_CLOSED => {}
            CAPACITY_TAKEN | CAPACITY_CANCELED => return,
            _invalid => {
                state.closed = true;
                signal
                    .status
                    .store(CAPACITY_CANCELED, AtomicOrdering::Release);
                let closed = Self::close_waiters_locked(&mut state);
                drop(state);
                Self::notify(closed);
                return;
            }
        }
        signal
            .status
            .store(CAPACITY_CANCELED, AtomicOrdering::Release);
        let ready = self.dispatch_locked(&mut state);
        drop(state);
        Self::notify(ready);
    }

    fn close(&self) {
        let signals = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.closed {
                return;
            }
            state.closed = true;
            Self::close_waiters_locked(&mut state)
        };
        Self::notify(signals);
    }

    fn return_capacity_locked(&self, state: &mut CapacityState, units: usize) -> bool {
        let Some(available) = state.available.checked_add(units) else {
            state.closed = true;
            return false;
        };
        if available > self.capacity {
            state.closed = true;
            return false;
        }
        state.available = available;
        true
    }

    fn close_waiters_locked(state: &mut CapacityState) -> Vec<Arc<CapacitySignal>> {
        state
            .waiters
            .drain(..)
            .map(|waiter| {
                waiter
                    .signal
                    .status
                    .store(CAPACITY_CLOSED, AtomicOrdering::Release);
                waiter.signal
            })
            .collect()
    }

    fn dispatch_locked(&self, state: &mut CapacityState) -> Vec<Arc<CapacitySignal>> {
        if state.closed {
            return Vec::new();
        }
        let mut ready = Vec::new();
        loop {
            let mut selected = None;
            for (index, waiter) in state.waiters.iter().enumerate() {
                if waiter.units <= state.available {
                    let exceeds_bypass_budget = state.waiters.iter().take(index).any(|older| {
                        older.bypassed_units.saturating_add(waiter.units) > self.capacity
                    });
                    if exceeds_bypass_budget {
                        break;
                    }
                    selected = Some(index);
                    break;
                }
                if waiter.bypassed_units >= self.capacity {
                    break;
                }
            }
            let Some(index) = selected else {
                break;
            };
            let granted_units = state.waiters[index].units;
            for waiter in state.waiters.iter_mut().take(index) {
                waiter.bypassed_units = waiter.bypassed_units.saturating_add(granted_units);
            }
            let Some(waiter) = state.waiters.remove(index) else {
                state.closed = true;
                ready.extend(Self::close_waiters_locked(state));
                break;
            };
            state.available -= waiter.units;
            waiter
                .signal
                .status
                .store(CAPACITY_GRANTED, AtomicOrdering::Release);
            ready.push(waiter.signal);
        }
        ready
    }

    fn notify(signals: Vec<Arc<CapacitySignal>>) {
        for signal in signals {
            signal.changed.notify_waiters();
        }
    }

    #[cfg(test)]
    fn available(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .available
    }
}

struct OwnershipPermit {
    broker: Arc<CapacityBroker>,
    units: usize,
}

impl OwnershipPermit {
    fn new(broker: Arc<CapacityBroker>, units: usize) -> Self {
        Self { broker, units }
    }

    fn split_one(&mut self) -> Option<Self> {
        if self.units == 0 {
            return None;
        }
        self.units -= 1;
        Some(Self::new(Arc::clone(&self.broker), 1))
    }

    fn close_and_forget(self) {
        self.broker.close();
        std::mem::forget(self);
    }
}

impl Drop for OwnershipPermit {
    fn drop(&mut self) {
        let units = std::mem::take(&mut self.units);
        self.broker.release(units);
    }
}

struct DropBatch {
    permit: Option<OwnershipPermit>,
    jobs: Vec<DropJob>,
    panic_reporter: Option<PanicReporter>,
    poisoned: bool,
}

impl DropBatch {
    fn new(
        permit: OwnershipPermit,
        jobs: Vec<DropJob>,
        panic_reporter: Option<PanicReporter>,
        poisoned: bool,
    ) -> Self {
        Self {
            permit: Some(permit),
            jobs,
            panic_reporter,
            poisoned,
        }
    }

    fn run(mut self) {
        let jobs = std::mem::take(&mut self.jobs);
        let mut clean = true;
        for job in jobs {
            if let Err(payload) = catch_unwind(AssertUnwindSafe(job)) {
                clean = false;
                // A hostile panic payload may itself panic in Drop. It remains
                // charged to the permanently retained permit for this bundle.
                let message = panic_message(&payload);
                std::mem::forget(payload);
                if let Some(report) = self.panic_reporter.take()
                    && let Err(report_panic) = catch_unwind(AssertUnwindSafe(|| report(message)))
                {
                    std::mem::forget(report_panic);
                }
            }
        }
        if clean && !self.poisoned {
            // Release the permit only after every charged destructor in this
            // bundle returned normally.
            drop(self.permit.take());
        } else if let Some(permit) = self.permit.take() {
            // The permit itself is the fixed-size accounting record for this
            // hostile bundle. Retaining it permanently prevents repeated
            // uncharged ownership from entering the executor. Closing the
            // semaphore wakes every admission waiter with a typed capacity
            // error instead of leaving it parked behind a poisoned slot.
            permit.close_and_forget();
        }
    }
}

fn panic_message(payload: &Box<dyn Any + Send>) -> String {
    payload
        .downcast_ref::<&'static str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_owned())
}

impl Drop for DropBatch {
    fn drop(&mut self) {
        // A disconnected worker set cannot execute this already charged batch.
        // Retain both the bounded payload and its permit; future acquisition is
        // closed by `DropExecutor::submit`.
        if let Some(permit) = self.permit.take() {
            permit.close_and_forget();
        }
        if !self.jobs.is_empty() {
            std::mem::forget(std::mem::take(&mut self.jobs));
        }
    }
}

struct DropExecutor {
    sender: Option<Sender<DropBatch>>,
    capacity: Arc<CapacityBroker>,
}

impl DropExecutor {
    fn start(worker_count: usize, capacity: usize) -> Arc<Self> {
        let (sender, receiver) = channel::<DropBatch>();
        let receiver = Arc::new(Mutex::new(receiver));
        let mut started = 0usize;

        for index in 0..worker_count {
            let receiver = Arc::clone(&receiver);
            if std::thread::Builder::new()
                .name(format!("taskvisor-drop-{index}"))
                .spawn(move || worker_loop(&receiver))
                .is_ok()
            {
                started += 1;
            }
        }

        let capacity = CapacityBroker::new(capacity);
        let sender = if started == 0 {
            capacity.close();
            None
        } else {
            Some(sender)
        };
        Arc::new(Self { sender, capacity })
    }

    async fn reserve(self: &Arc<Self>) -> Result<DropReservation, DropCapacityError> {
        let permit = self.capacity.acquire(1).await?;
        Ok(DropReservation {
            executor: Arc::clone(self),
            permit: Some(permit),
        })
    }

    fn try_reserve(self: &Arc<Self>) -> Result<DropReservation, DropCapacityError> {
        let permit = self.capacity.try_acquire(1)?;
        Ok(DropReservation {
            executor: Arc::clone(self),
            permit: Some(permit),
        })
    }

    #[cfg(test)]
    async fn reserve_many(
        self: &Arc<Self>,
        count: usize,
    ) -> Result<Vec<DropReservation>, DropCapacityError> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let mut combined = self.capacity.acquire(count).await?;
        let mut reservations = Vec::with_capacity(count);
        for _ in 0..count {
            let permit = combined
                .split_one()
                .expect("the atomic reservation contains the requested permits");
            reservations.push(DropReservation {
                executor: Arc::clone(self),
                permit: Some(permit),
            });
        }
        Ok(reservations)
    }

    fn try_reserve_many(
        self: &Arc<Self>,
        count: usize,
    ) -> Result<Vec<DropReservation>, DropCapacityError> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let mut combined = self.capacity.try_acquire(count)?;
        let mut reservations = Vec::with_capacity(count);
        for _ in 0..count {
            let permit = combined
                .split_one()
                .expect("the atomic reservation contains the requested permits");
            reservations.push(DropReservation {
                executor: Arc::clone(self),
                permit: Some(permit),
            });
        }
        Ok(reservations)
    }

    fn submit(&self, batch: DropBatch) {
        let Some(sender) = &self.sender else {
            // No worker ever started. ManuallyDrop keeps this one charged batch
            // bounded, and the closed semaphore rejects every future owner.
            std::mem::forget(batch);
            return;
        };
        if let Err(error) = sender.send(batch) {
            // Every worker disappeared unexpectedly. Close admission before
            // retaining the already charged batch.
            self.capacity.close();
            std::mem::forget(error.0);
        }
    }
}

fn worker_loop(receiver: &Mutex<Receiver<DropBatch>>) {
    loop {
        let batch = receiver
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .recv();
        let Ok(batch) = batch else {
            return;
        };
        batch.run();
    }
}

fn executor() -> &'static Arc<DropExecutor> {
    static EXECUTOR: OnceLock<Arc<DropExecutor>> = OnceLock::new();
    EXECUTOR.get_or_init(|| DropExecutor::start(WORKER_COUNT, OWNERSHIP_CAPACITY))
}

/// Waits for one globally charged user-lifetime ownership slot.
pub(crate) async fn reserve() -> Result<DropReservation, DropCapacityError> {
    executor().reserve().await
}

/// Atomically acquires several user-lifetime ownership slots without waiting.
pub(crate) fn try_reserve_many(count: usize) -> Result<Vec<DropReservation>, DropCapacityError> {
    if count == 0 {
        return Ok(Vec::new());
    }
    executor().try_reserve_many(count)
}

/// Acquires one ownership slot without waiting.
pub(crate) fn try_reserve() -> Result<DropReservation, DropCapacityError> {
    executor().try_reserve()
}

#[cfg(test)]
pub(crate) fn test_reservation() -> DropReservation {
    static TEST_EXECUTOR: OnceLock<Arc<DropExecutor>> = OnceLock::new();
    TEST_EXECUTOR
        .get_or_init(|| DropExecutor::start(2, 16_384))
        .try_reserve()
        .expect("the shared test destructor executor has sufficient ownership slots")
}

#[cfg(test)]
pub(crate) fn isolated_test_reservation() -> DropReservation {
    DropExecutor::start(1, 1)
        .try_reserve()
        .expect("a fresh isolated test executor has one ownership slot")
}

/// Isolated ownership admission used by deterministic saturation tests.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct TestReservationSource(Arc<DropExecutor>);

#[cfg(test)]
impl TestReservationSource {
    pub(crate) fn new(capacity: usize) -> Self {
        Self(DropExecutor::start(1, capacity))
    }

    pub(crate) async fn reserve(&self) -> Result<DropReservation, DropCapacityError> {
        self.0.reserve().await
    }

    pub(crate) fn try_reserve_many(
        &self,
        count: usize,
    ) -> Result<Vec<DropReservation>, DropCapacityError> {
        self.0.try_reserve_many(count)
    }

    pub(crate) fn try_reserve(&self) -> Result<DropReservation, DropCapacityError> {
        self.0.try_reserve()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Condvar,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;
    use std::{future::Future, pin::Pin, task::Poll};

    #[derive(Default)]
    struct GateState {
        entered: bool,
        released: bool,
    }

    struct BlockingDrop(Arc<(Mutex<GateState>, Condvar)>);

    impl Drop for BlockingDrop {
        fn drop(&mut self) {
            let (state, ready) = &*self.0;
            let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
            state.entered = true;
            ready.notify_all();
            while !state.released {
                state = ready.wait(state).unwrap_or_else(|error| error.into_inner());
            }
        }
    }

    async fn assert_pending_once<F: Future>(mut future: Pin<&mut F>) {
        std::future::poll_fn(|context| match future.as_mut().poll(context) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("the ownership request completed before capacity existed"),
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn batch_reservation_is_atomic_and_rejects_process_limit_overflow() {
        let executor = DropExecutor::start(1, 2);
        let held = executor.try_reserve().expect("one initial slot");
        let mut batch = Box::pin(executor.reserve_many(2));
        assert_pending_once(batch.as_mut()).await;
        drop(batch);
        assert_eq!(
            executor.capacity.available(),
            1,
            "canceling an unsatisfied atomic batch must return every internally held permit"
        );

        let batch = executor.reserve_many(2);
        drop(held);
        let reservations = batch.await.expect("both slots become available together");
        assert_eq!(reservations.len(), 2);
        drop(reservations);
        assert_eq!(executor.capacity.available(), 2);

        assert!(
            executor.reserve_many(OWNERSHIP_CAPACITY + 1).await.is_err(),
            "a batch larger than the process budget must fail without waiting"
        );
    }

    #[test]
    fn try_batch_reservation_is_atomic_and_returns_partial_capacity() {
        let executor = DropExecutor::start(1, 2);
        let held = executor.try_reserve().expect("one initial slot");

        assert!(
            executor.try_reserve_many(2).is_err(),
            "a fail-fast batch cannot consume only the one available slot"
        );
        assert_eq!(executor.capacity.available(), 1);

        drop(held);
        let reservations = executor
            .try_reserve_many(2)
            .expect("the full batch fits after both slots are available");
        assert_eq!(reservations.len(), 2);
        drop(reservations);
        assert_eq!(executor.capacity.available(), 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unsatisfied_atomic_batch_does_not_block_single_slot_admission() {
        let executor = DropExecutor::start(1, 2);
        let held = executor.try_reserve().expect("one initial slot");
        let mut batch = Box::pin(executor.reserve_many(2));
        assert_pending_once(batch.as_mut()).await;

        let single = tokio::time::timeout(Duration::from_secs(1), executor.reserve())
            .await
            .expect("an unsatisfied batch must not head-of-line block a usable slot")
            .expect("the ownership executor remains open");
        assert_pending_once(batch.as_mut()).await;

        drop(single);
        assert_pending_once(batch.as_mut()).await;
        drop(held);
        let reservations = tokio::time::timeout(Duration::from_secs(1), batch)
            .await
            .expect("the atomic batch must wake when its full capacity becomes available")
            .expect("the ownership executor remains open");
        assert_eq!(reservations.len(), 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn waiting_atomic_batch_gets_priority_after_bounded_single_bypass() {
        let executor = DropExecutor::start(1, 2);
        let held = executor.try_reserve().expect("one initial slot");
        let mut batch = Box::pin(executor.reserve_many(2));
        assert_pending_once(batch.as_mut()).await;

        for _ in 0..2 {
            let single = tokio::time::timeout(Duration::from_secs(1), executor.reserve())
                .await
                .expect("one capacity turnover may bypass the unsatisfied batch")
                .expect("the ownership executor remains open");
            drop(single);
        }

        let mut next_single = Box::pin(executor.reserve());
        assert_pending_once(next_single.as_mut()).await;
        drop(held);

        let reservations = tokio::time::timeout(Duration::from_secs(1), batch)
            .await
            .expect("bounded bypass must let the older atomic batch accumulate capacity")
            .expect("the ownership executor remains open");
        assert_pending_once(next_single.as_mut()).await;

        drop(reservations);
        tokio::time::timeout(Duration::from_secs(1), next_single)
            .await
            .expect("single-slot admission resumes after the fair batch grant")
            .expect("the ownership executor remains open");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pending_admission_metadata_is_bounded_by_broker_capacity() {
        let executor = DropExecutor::start(1, 1);
        let held = executor.try_reserve().expect("one initial slot");
        let mut first = Box::pin(executor.reserve());
        assert_pending_once(first.as_mut()).await;

        assert!(
            executor.reserve().await.is_err(),
            "a second pending request must be rejected at the waiter budget"
        );
        drop(first);
        drop(held);
        assert_eq!(executor.capacity.available(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn canceling_a_notified_grant_returns_its_capacity() {
        let executor = DropExecutor::start(1, 1);
        let held = executor.try_reserve().expect("the only slot");
        let mut waiter = Box::pin(executor.reserve());
        assert_pending_once(waiter.as_mut()).await;

        drop(held);
        drop(waiter);

        let recovered = executor
            .try_reserve()
            .expect("canceling a granted but unobserved waiter must return its slot");
        drop(recovered);
        assert_eq!(executor.capacity.available(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reservation_bounds_running_queued_and_pre_submit_ownership() {
        let executor = DropExecutor::start(1, 2);
        let gate = Arc::new((Mutex::new(GateState::default()), Condvar::new()));
        executor
            .try_reserve()
            .expect("first slot")
            .bundle(BlockingDrop(Arc::clone(&gate)))
            .submit();
        executor
            .try_reserve()
            .expect("second slot")
            .bundle(())
            .submit();

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if gate
                    .0
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .entered
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the worker must enter the blocking destructor");

        for _ in 0..128 {
            assert!(
                executor.try_reserve().is_err(),
                "saturation returns ownership to the pre-transfer caller"
            );
        }

        {
            let (state, ready) = &*gate;
            state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .released = true;
            ready.notify_all();
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if executor.capacity.available() == 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("clean bundle completion must return both reservations");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn panicking_destructor_permanently_consumes_only_its_charged_slot() {
        struct PanickingDrop(Arc<AtomicBool>);

        impl Drop for PanickingDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
                panic!("injected destructor panic");
            }
        }

        let executor = DropExecutor::start(1, 1);
        let attempted = Arc::new(AtomicBool::new(false));
        let reported = Arc::new(AtomicBool::new(false));
        let mut bundle = executor
            .try_reserve()
            .expect("the only slot")
            .bundle(PanickingDrop(Arc::clone(&attempted)));
        let attempted_for_report = Arc::clone(&attempted);
        let reported_by_worker = Arc::clone(&reported);
        bundle.set_panic_reporter(move |message| {
            assert!(
                attempted_for_report.load(Ordering::Acquire),
                "the destructor panic must be caught before reporting"
            );
            assert!(message.contains("injected destructor panic"));
            reported_by_worker.store(true, Ordering::Release);
        });
        bundle.submit();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !reported.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the worker must report the hostile destructor");

        for _ in 0..128 {
            assert!(
                executor.try_reserve().is_err(),
                "the one charged failure must fail-close all later ownership"
            );
        }
        let waiter = tokio::time::timeout(Duration::from_secs(1), executor.reserve())
            .await
            .expect("closing poisoned ownership admission must wake async waiters");
        assert!(
            waiter.is_err(),
            "a poisoned destructor executor must reject async admission"
        );
        assert_eq!(executor.capacity.available(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn actor_cleanup_poison_closes_later_ownership_admission() {
        let executor = DropExecutor::start(1, 1);
        let mut bundle = executor.try_reserve().expect("the only slot").bundle(());
        bundle.poison();
        bundle.submit();

        let next = tokio::time::timeout(Duration::from_secs(1), executor.reserve())
            .await
            .expect("poisoned terminal cleanup must wake ownership admission");
        assert!(
            next.is_err(),
            "a retained nested panic payload must fail-close later admission"
        );
        assert_eq!(executor.capacity.available(), 0);
    }
}
