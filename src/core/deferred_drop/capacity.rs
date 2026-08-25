//! Limits how many user-owned values one supervisor can retain.
//!
//! [`CapacityBroker`] sits between a started cleanup executor and [`DropReservation`](super::bundle::DropReservation) creation.
//! Waiting admission requests one unit in FIFO order. Fail-fast batches receive every requested unit or none.
//! Dropping a healthy [`OwnershipPermit`] returns its units.
//! Cleanup that panics or is marked poisoned retires them.

use std::{
    collections::VecDeque,
    num::NonZeroUsize,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering as AtomicOrdering},
    },
};

use tokio::sync::Notify;

use super::error::DropCapacityError;

/// A request remains in the broker queue.
const CAPACITY_WAITING: u8 = 0;

/// A complete grant waits for its request future to observe it.
const CAPACITY_GRANTED: u8 = 1;

/// The broker closed before the future took its grant.
const CAPACITY_CLOSED: u8 = 2;

/// The request future converted its grant into a permit.
const CAPACITY_TAKEN: u8 = 3;

/// Cancellation removed a queued request or returned an unobserved grant.
const CAPACITY_CANCELED: u8 = 4;

/// Atomic result state and wake signal for one waiting future.
struct CapacitySignal {
    /// One of the capacity state constants above.
    status: AtomicU8,
    /// Wakes the future after a grant or closure.
    changed: Notify,
}

impl CapacitySignal {
    /// Initializes a signal before the request enters the queue.
    fn waiting() -> Self {
        Self {
            status: AtomicU8::new(CAPACITY_WAITING),
            changed: Notify::new(),
        }
    }
}

/// Mutable accounting for one configured ownership limit.
struct LimitedCapacityState {
    /// Units that are not granted or held by permits.
    available: usize,
    /// Units that remain usable after permanent retirement.
    effective_capacity: usize,
    /// Pending requests in arrival order.
    waiters: VecDeque<Arc<CapacitySignal>>,
}

/// Accounting selected by the public ownership-capacity setting.
enum CapacityMode {
    /// Admission follows the configured finite limit.
    Limited(LimitedCapacityState),
    /// Every non-zero request is admitted without capacity accounting.
    Unlimited,
}

/// Mutable admission state protected by the broker mutex.
struct CapacityState {
    /// Whether the broker rejects all new requests.
    closed: bool,
    /// Finite accounting or explicit unlimited admission.
    mode: CapacityMode,
}

/// Cancellation-safe single-unit FIFO admission plus atomic fail-fast batches.
pub(super) struct CapacityBroker {
    /// Original finite limit, or `None` for unlimited admission.
    limit: Option<NonZeroUsize>,
    /// Admission mode, closure, and any finite-capacity waiters.
    state: Mutex<CapacityState>,
    /// Best-effort callback invoked after finite capacity is permanently reduced.
    retirement_reporter: Mutex<Option<RetirementReporter>>,
}

/// One committed reduction of finite ownership capacity.
#[derive(Clone, Copy)]
pub(super) struct CapacityRetirement {
    /// Original configured finite limit.
    pub(super) configured_capacity: usize,
    /// Remaining usable capacity after this transition.
    pub(super) effective_capacity: usize,
    /// Units removed by this transition.
    pub(super) retired_units: usize,
}

/// Shared diagnostic callback for committed ownership retirement.
pub(super) type RetirementReporter = Arc<dyn Fn(CapacityRetirement) + Send + Sync + 'static>;

/// Point-in-time finite or unlimited broker accounting.
pub(super) struct CapacitySnapshot {
    /// Original finite limit, or `None` for unlimited admission.
    pub(super) configured_limit: Option<usize>,
    /// Post-retirement finite limit, or `None` for unlimited admission.
    pub(super) effective_limit: Option<usize>,
    /// Currently uncharged finite units, or `None` for unlimited admission.
    pub(super) available: Option<usize>,
    /// Requests still parked in the finite-capacity queue.
    pub(super) waiters: usize,
    /// Whether the broker accepts new requests.
    pub(super) open: bool,
}

/// Result of attempting admission before a future waits.
enum CapacityStart {
    /// The complete permit was available without queueing.
    Ready(OwnershipPermit),
    /// The request entered the bounded queue.
    Waiting(CapacityRequest),
}

/// Wait state that returns an abandoned grant to the broker.
struct CapacityRequest {
    /// Broker that owns the request queue.
    broker: Arc<CapacityBroker>,
    /// Result state registered with the broker queue.
    signal: Arc<CapacitySignal>,
    /// Whether `Drop` must remove the request or return its grant.
    active: bool,
}

impl CapacityRequest {
    /// Converts a complete asynchronous grant into a permit.
    ///
    /// The notification is enabled before reading the atomic state.
    /// This keeps a concurrent grant or close from being missed.
    ///
    /// # Errors
    ///
    /// Returns an error when admission closes before the complete grant is taken.
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
                    return Ok(OwnershipPermit::new(Arc::clone(&self.broker), 1));
                }
                CAPACITY_CLOSED => {
                    self.active = false;
                    return Err(self.broker.error());
                }
                _invalid => {
                    self.broker.close();
                    self.active = false;
                    return Err(self.broker.error());
                }
            }
        }
    }
}

impl Drop for CapacityRequest {
    /// Returns any grant that was not transferred into a permit.
    fn drop(&mut self) {
        if self.active {
            self.broker.cancel(&self.signal);
        }
    }
}

impl CapacityBroker {
    /// Opens a broker in finite or unlimited admission mode.
    pub(super) fn new(limit: Option<NonZeroUsize>) -> Arc<Self> {
        let mode = match limit {
            Some(limit) => CapacityMode::Limited(LimitedCapacityState {
                available: limit.get(),
                effective_capacity: limit.get(),
                waiters: VecDeque::new(),
            }),
            None => CapacityMode::Unlimited,
        };
        Arc::new(Self {
            limit,
            state: Mutex::new(CapacityState {
                closed: false,
                mode,
            }),
            retirement_reporter: Mutex::new(None),
        })
    }

    /// Installs or replaces the best-effort retirement callback.
    pub(super) fn set_retirement_reporter(&self, reporter: RetirementReporter) {
        *self
            .retirement_reporter
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(reporter);
    }

    /// Builds the typed rejection for this broker's admission mode.
    fn error(&self) -> DropCapacityError {
        DropCapacityError::new(self.limit)
    }

    /// Waits until one unit can move into a permit.
    ///
    /// # Errors
    ///
    /// Returns an error for a closed broker, exhausted effective capacity, or a full waiter queue.
    pub(super) async fn acquire_one(
        self: &Arc<Self>,
    ) -> Result<OwnershipPermit, DropCapacityError> {
        match self.start_acquire_one()? {
            CapacityStart::Ready(permit) => Ok(permit),
            CapacityStart::Waiting(request) => request.wait().await,
        }
    }

    /// Grants the complete request only when it can proceed immediately.
    ///
    /// # Errors
    ///
    /// Returns an error when the request is invalid, admission is closed, an
    /// older request is waiting, or the complete capacity is unavailable.
    pub(super) fn try_acquire(
        self: &Arc<Self>,
        units: usize,
    ) -> Result<OwnershipPermit, DropCapacityError> {
        if units == 0 || self.limit.is_some_and(|limit| units > limit.get()) {
            return Err(self.error());
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed {
            return Err(self.error());
        }
        let CapacityMode::Limited(limited) = &mut state.mode else {
            return Ok(OwnershipPermit::new(Arc::clone(self), units));
        };
        if units > limited.effective_capacity
            || !limited.waiters.is_empty()
            || limited.available < units
        {
            return Err(self.error());
        }
        limited.available -= units;
        Ok(OwnershipPermit::new(Arc::clone(self), units))
    }

    /// Chooses immediate admission or registers one cancellation-safe waiter.
    ///
    /// Pending request metadata is limited to the original broker capacity.
    ///
    /// # Errors
    ///
    /// Returns an error for a closed broker, exhausted effective capacity, or a full waiter queue.
    fn start_acquire_one(self: &Arc<Self>) -> Result<CapacityStart, DropCapacityError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed {
            return Err(self.error());
        }
        let CapacityMode::Limited(limited) = &mut state.mode else {
            return Ok(CapacityStart::Ready(OwnershipPermit::new(
                Arc::clone(self),
                1,
            )));
        };
        if limited.effective_capacity == 0 {
            return Err(self.error());
        }
        if limited.waiters.is_empty() && limited.available > 0 {
            limited.available -= 1;
            return Ok(CapacityStart::Ready(OwnershipPermit::new(
                Arc::clone(self),
                1,
            )));
        }
        let capacity = self
            .limit
            .expect("limited capacity mode has a configured limit")
            .get();
        if limited.waiters.len() >= capacity {
            return Err(self.error());
        }

        let signal = Arc::new(CapacitySignal::waiting());
        limited.waiters.push_back(Arc::clone(&signal));
        drop(state);
        Ok(CapacityStart::Waiting(CapacityRequest {
            broker: Arc::clone(self),
            signal,
            active: true,
        }))
    }

    /// Returns healthy units and dispatches newly feasible requests.
    fn release(&self, units: usize) {
        if units == 0 {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let ready = if Self::return_capacity_locked(&mut state, units) {
            Self::dispatch_locked(&mut state)
        } else {
            Self::close_waiters_locked(&mut state)
        };
        drop(state);
        Self::notify(ready);
    }

    /// Permanently removes units whose cleanup cannot safely return them.
    ///
    /// Single-unit waiters continue against any remaining effective capacity.
    fn retire(&self, units: usize) {
        if units == 0 {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed {
            return;
        }
        let CapacityMode::Limited(limited) = &mut state.mode else {
            return;
        };
        let mut signals = Vec::new();
        let mut retirement = None;
        let close = if let Some(effective_capacity) = limited.effective_capacity.checked_sub(units)
        {
            if limited.available > effective_capacity {
                true
            } else {
                limited.effective_capacity = effective_capacity;
                retirement = Some(CapacityRetirement {
                    configured_capacity: self
                        .limit
                        .expect("limited capacity mode has a configured limit")
                        .get(),
                    effective_capacity,
                    retired_units: units,
                });
                effective_capacity == 0
            }
        } else {
            true
        };
        if close {
            state.closed = true;
            signals.extend(Self::close_waiters_locked(&mut state));
        } else {
            signals.extend(Self::dispatch_locked(&mut state));
        }
        drop(state);
        Self::notify(signals);
        if let Some(retirement) = retirement {
            self.report_retirement(retirement);
        }
    }

    /// Invokes the diagnostic callback outside broker accounting locks.
    fn report_retirement(&self, retirement: CapacityRetirement) {
        let reporter = self
            .retirement_reporter
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(reporter) = reporter
            && let Err(payload) = catch_unwind(AssertUnwindSafe(|| reporter(retirement)))
        {
            std::mem::forget(payload);
        }
    }

    /// Removes a waiting request or returns its unobserved grant.
    fn cancel(&self, signal: &Arc<CapacitySignal>) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match signal.status.load(AtomicOrdering::Acquire) {
            CAPACITY_WAITING => {
                let CapacityMode::Limited(limited) = &mut state.mode else {
                    state.closed = true;
                    signal
                        .status
                        .store(CAPACITY_CANCELED, AtomicOrdering::Release);
                    return;
                };
                if let Some(position) = limited
                    .waiters
                    .iter()
                    .position(|waiter| Arc::ptr_eq(waiter, signal))
                {
                    limited.waiters.remove(position);
                }
            }
            CAPACITY_GRANTED => {
                if !Self::return_capacity_locked(&mut state, 1) {
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
        let ready = Self::dispatch_locked(&mut state);
        drop(state);
        Self::notify(ready);
    }

    /// Rejects future admission and wakes every queued request.
    pub(super) fn close(&self) {
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

    /// Adds returned units while enforcing the effective limit.
    fn return_capacity_locked(state: &mut CapacityState, units: usize) -> bool {
        let CapacityMode::Limited(limited) = &mut state.mode else {
            return true;
        };
        let Some(available) = limited.available.checked_add(units) else {
            state.closed = true;
            return false;
        };
        if available > limited.effective_capacity {
            state.closed = true;
            return false;
        }
        limited.available = available;
        true
    }

    /// Marks every queued request closed before removing it.
    fn close_waiters_locked(state: &mut CapacityState) -> Vec<Arc<CapacitySignal>> {
        let CapacityMode::Limited(limited) = &mut state.mode else {
            return Vec::new();
        };
        limited
            .waiters
            .drain(..)
            .inspect(|signal| {
                signal
                    .status
                    .store(CAPACITY_CLOSED, AtomicOrdering::Release);
            })
            .collect()
    }

    /// Grants queued single-unit requests from the front while capacity exists.
    fn dispatch_locked(state: &mut CapacityState) -> Vec<Arc<CapacitySignal>> {
        if state.closed {
            return Vec::new();
        }
        let CapacityMode::Limited(limited) = &mut state.mode else {
            return Vec::new();
        };
        Self::dispatch_limited(limited)
    }

    /// Dispatches finite-capacity waiters in FIFO order.
    fn dispatch_limited(limited: &mut LimitedCapacityState) -> Vec<Arc<CapacitySignal>> {
        let mut ready = Vec::new();
        while limited.available > 0 {
            let Some(signal) = limited.waiters.pop_front() else {
                break;
            };
            limited.available -= 1;
            signal
                .status
                .store(CAPACITY_GRANTED, AtomicOrdering::Release);
            ready.push(signal);
        }
        ready
    }

    /// Sends wakeups after the caller releases the broker mutex.
    fn notify(signals: Vec<Arc<CapacitySignal>>) {
        for signal in signals {
            signal.changed.notify_waiters();
        }
    }

    /// Copies current admission accounting under the broker mutex.
    pub(super) fn snapshot(&self) -> CapacitySnapshot {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match &state.mode {
            CapacityMode::Limited(limited) => CapacitySnapshot {
                configured_limit: self.limit.map(NonZeroUsize::get),
                effective_limit: Some(limited.effective_capacity),
                available: Some(limited.available),
                waiters: limited.waiters.len(),
                open: !state.closed,
            },
            CapacityMode::Unlimited => CapacitySnapshot {
                configured_limit: None,
                effective_limit: None,
                available: None,
                waiters: 0,
                open: !state.closed,
            },
        }
    }

    /// Reports currently uncharged units to white-box tests.
    #[cfg(test)]
    pub(super) fn available(&self) -> usize {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let CapacityMode::Limited(limited) = &state.mode else {
            panic!("unlimited capacity has no available-unit count");
        };
        limited.available
    }

    /// Reports the post-retirement limit to white-box tests.
    #[cfg(test)]
    pub(super) fn effective_capacity(&self) -> usize {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let CapacityMode::Limited(limited) = &state.mode else {
            panic!("unlimited capacity has no effective-unit count");
        };
        limited.effective_capacity
    }

    /// Reports the number of queued finite-capacity waiters to white-box tests.
    #[cfg(test)]
    pub(super) fn waiter_count(&self) -> usize {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match &state.mode {
            CapacityMode::Limited(limited) => limited.waiters.len(),
            CapacityMode::Unlimited => 0,
        }
    }
}

/// Capacity charged to one atomic admission result.
pub(super) struct OwnershipPermit {
    /// Broker that must receive release or retirement.
    broker: Arc<CapacityBroker>,
    /// Units still owned by this permit.
    units: usize,
}

impl OwnershipPermit {
    /// Records an all-or-nothing grant from the broker.
    fn new(broker: Arc<CapacityBroker>, units: usize) -> Self {
        Self { broker, units }
    }

    /// Moves one unit into an independent permit.
    pub(super) fn split_one(&mut self) -> Option<Self> {
        if self.units == 0 {
            return None;
        }
        self.units -= 1;
        Some(Self::new(Arc::clone(&self.broker), 1))
    }

    /// Retires every remaining unit in this permit.
    pub(super) fn retire(mut self) {
        let units = std::mem::take(&mut self.units);
        self.broker.retire(units);
    }

    /// Closes admission when this permit cannot reach a cleanup worker.
    pub(super) fn close_without_release(mut self) {
        self.units = 0;
        self.broker.close();
    }
}

impl Drop for OwnershipPermit {
    /// Returns every remaining healthy unit to the broker.
    fn drop(&mut self) {
        let units = std::mem::take(&mut self.units);
        self.broker.release(units);
    }
}
