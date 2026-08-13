//! Limits how many user-owned values one supervisor can retain.
//!
//! [`CapacityBroker`] sits between a started cleanup executor and [`DropReservation`](super::bundle::DropReservation) creation.
//! A request receives every requested unit or none. Dropping a healthy [`OwnershipPermit`] returns its units.
//! Cleanup that panics or is marked poisoned retires them.

use std::{
    collections::VecDeque,
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

/// Queue entry for one all-or-nothing request.
struct CapacityWaiter {
    /// Number of units that must be granted together.
    units: usize,
    /// Units granted past this request while it could not fit.
    bypassed_units: usize,
    /// Result state shared with the waiting future.
    signal: Arc<CapacitySignal>,
}

/// Mutable admission state protected by the broker mutex.
struct CapacityState {
    /// Units that are not granted or held by permits.
    available: usize,
    /// Units that remain usable after permanent retirement.
    effective_capacity: usize,
    /// Whether the broker rejects all new requests.
    closed: bool,
    /// Pending requests in arrival order.
    waiters: VecDeque<CapacityWaiter>,
}

/// Cancellation-safe ownership admission with bounded bypass.
///
/// An older request may be bypassed while it cannot fit. Each older request counts the units granted past it.
/// Newer requests may pass only while that count stays within the current effective capacity.
/// Once the count reaches the limit, released capacity accumulates for the older request.
pub(super) struct CapacityBroker {
    /// Original unit limit and maximum number of queued requests.
    capacity: usize,
    /// Available units, effective limit, closure, and queued requests.
    state: Mutex<CapacityState>,
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
    /// Units that must transfer together.
    units: usize,
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
                    return Ok(OwnershipPermit::new(Arc::clone(&self.broker), self.units));
                }
                CAPACITY_CLOSED => {
                    self.active = false;
                    return Err(DropCapacityError::new(self.broker.capacity));
                }
                _invalid => {
                    self.broker.close();
                    self.active = false;
                    return Err(DropCapacityError::new(self.broker.capacity));
                }
            }
        }
    }
}

impl Drop for CapacityRequest {
    /// Returns any grant that was not transferred into a permit.
    fn drop(&mut self) {
        if self.active {
            self.broker.cancel(self.units, &self.signal);
        }
    }
}

impl CapacityBroker {
    /// Opens a broker with its full configured budget available.
    pub(super) fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            capacity,
            state: Mutex::new(CapacityState {
                available: capacity,
                effective_capacity: capacity,
                closed: false,
                waiters: VecDeque::new(),
            }),
        })
    }

    /// Waits until every requested unit can move into one permit.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid request, a closed broker, an impossible
    /// request after capacity retirement, or a full waiter queue.
    pub(super) async fn acquire(
        self: &Arc<Self>,
        units: usize,
    ) -> Result<OwnershipPermit, DropCapacityError> {
        match self.start_acquire(units)? {
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
        if units == 0 || units > self.capacity {
            return Err(DropCapacityError::new(self.capacity));
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed
            || units > state.effective_capacity
            || !state.waiters.is_empty()
            || state.available < units
        {
            return Err(DropCapacityError::new(self.capacity));
        }
        state.available -= units;
        Ok(OwnershipPermit::new(Arc::clone(self), units))
    }

    /// Chooses immediate admission or registers one cancellation-safe waiter.
    ///
    /// Pending request metadata is limited to the original broker capacity.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid or impossible request, a closed broker,
    /// or a full waiter queue.
    fn start_acquire(self: &Arc<Self>, units: usize) -> Result<CapacityStart, DropCapacityError> {
        if units == 0 || units > self.capacity {
            return Err(DropCapacityError::new(self.capacity));
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed || units > state.effective_capacity {
            return Err(DropCapacityError::new(self.capacity));
        }
        if state.waiters.is_empty() && state.available >= units {
            state.available -= units;
            return Ok(CapacityStart::Ready(OwnershipPermit::new(
                Arc::clone(self),
                units,
            )));
        }
        if state.waiters.len() >= self.capacity {
            return Err(DropCapacityError::new(self.capacity));
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

    /// Returns healthy units and dispatches newly feasible requests.
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

    /// Permanently removes units whose cleanup cannot safely return them.
    ///
    /// Requests above the new effective limit are rejected.
    /// Feasible requests continue against the remaining units.
    fn retire(&self, units: usize) {
        if units == 0 {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed {
            return;
        }
        let Some(effective_capacity) = state.effective_capacity.checked_sub(units) else {
            state.closed = true;
            let closed = Self::close_waiters_locked(&mut state);
            drop(state);
            Self::notify(closed);
            return;
        };
        if state.available > effective_capacity {
            state.closed = true;
            let closed = Self::close_waiters_locked(&mut state);
            drop(state);
            Self::notify(closed);
            return;
        }
        state.effective_capacity = effective_capacity;

        let mut rejected = Vec::new();
        state.waiters.retain(|waiter| {
            if waiter.units <= effective_capacity {
                true
            } else {
                waiter
                    .signal
                    .status
                    .store(CAPACITY_CLOSED, AtomicOrdering::Release);
                rejected.push(Arc::clone(&waiter.signal));
                false
            }
        });

        if effective_capacity == 0 {
            state.closed = true;
            rejected.extend(Self::close_waiters_locked(&mut state));
        } else {
            rejected.extend(self.dispatch_locked(&mut state));
        }
        drop(state);
        Self::notify(rejected);
    }

    /// Removes a waiting request or returns its unobserved grant.
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
    fn return_capacity_locked(&self, state: &mut CapacityState, units: usize) -> bool {
        let Some(available) = state.available.checked_add(units) else {
            state.closed = true;
            return false;
        };
        if available > state.effective_capacity {
            state.closed = true;
            return false;
        }
        state.available = available;
        true
    }

    /// Marks every queued request closed before removing it.
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

    /// Grants feasible requests without exceeding each older bypass budget.
    fn dispatch_locked(&self, state: &mut CapacityState) -> Vec<Arc<CapacitySignal>> {
        if state.closed {
            return Vec::new();
        }
        let effective_capacity = state.effective_capacity;
        let mut ready = Vec::new();
        loop {
            let mut selected = None;
            for (index, waiter) in state.waiters.iter().enumerate() {
                if waiter.units <= state.available {
                    let exceeds_bypass_budget = state.waiters.iter().take(index).any(|older| {
                        older.bypassed_units.saturating_add(waiter.units) > effective_capacity
                    });
                    if exceeds_bypass_budget {
                        break;
                    }
                    selected = Some(index);
                    break;
                }
                if waiter.bypassed_units >= effective_capacity {
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

    /// Sends wakeups after the caller releases the broker mutex.
    fn notify(signals: Vec<Arc<CapacitySignal>>) {
        for signal in signals {
            signal.changed.notify_waiters();
        }
    }

    /// Reports currently uncharged units to white-box tests.
    #[cfg(test)]
    pub(super) fn available(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .available
    }

    /// Reports the post-retirement limit to white-box tests.
    #[cfg(test)]
    pub(super) fn effective_capacity(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .effective_capacity
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
