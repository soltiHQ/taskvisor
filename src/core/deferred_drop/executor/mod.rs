//! Runs isolated user destructors on supervisor-local worker threads.
//!
//! [`DropExecutor`] connects the ownership [`CapacityBroker`] to a [`WorkerQueue`]. Reservations keep the executor alive.
//! Submitted [`DropBatch`] values move from controller, registry, subscriber, and force-abort terminal paths to the worker queue.
//!
//! ```text
//! DropDomain
//!     ▼
//! DropExecutor
//!     ├── capacity ──► CapacityBroker ──► DropReservation
//!     └── cleanup ───► DropBatch ───────► WorkerQueue ──► worker
//!                                               ▼
//!                                         DropBatch::run
//!                                               ├── clean ────────────► return healthy permit
//!                                               └── poison or panic ──► retire permit
//! ```
//!
//! Worker startup, growth, and idle retirement live in `worker`.
//! Per-batch panic handling and permit disposition live in `batch`.

use std::{num::NonZeroUsize, sync::Arc};

use super::{
    bundle::DropReservation,
    capacity::{CapacityBroker, CapacitySnapshot, OwnershipPermit},
    error::{DropCapacityError, DropStartError},
};

mod batch;
mod worker;

pub(super) use batch::DropBatch;
pub(super) use worker::{CORE_WORKER_COUNT, WorkerSpawner, system_spawner};
use worker::{CleanupSnapshot, WorkerQueue, max_worker_count};
#[cfg(test)]
pub(super) use worker::{ELASTIC_IDLE_TIMEOUT, MAX_WORKER_COUNT, worker_loop};

/// Combined broker and cleanup-worker state for one started executor.
pub(super) struct ExecutorSnapshot {
    /// Ownership admission accounting.
    pub(super) capacity: CapacitySnapshot,
    /// Deferred-cleanup queue accounting.
    pub(super) cleanup: CleanupSnapshot,
}

/// Started cleanup runtime shared by one domain and its outstanding reservations.
pub(super) struct DropExecutor {
    /// Grants the units required before Taskvisor accepts user ownership.
    pub(super) capacity: Arc<CapacityBroker>,
    /// Receives terminal batches and runs them on operating-system threads.
    pub(super) workers: Arc<WorkerQueue>,
}

impl DropExecutor {
    /// Started core worker set bounded by the domain's worker ceiling.
    ///
    /// # Errors
    ///
    /// - [`DropStartError`] when a required core worker thread cannot be created;
    /// - [`DropStartError`] when a required core worker exits before reporting ready.
    pub(super) fn try_start_with(
        worker_count: usize,
        capacity: Option<NonZeroUsize>,
        spawner: Arc<WorkerSpawner>,
    ) -> Result<Arc<Self>, DropStartError> {
        let max_workers = max_worker_count(capacity);
        let core_workers = worker_count.min(max_workers);
        let workers = WorkerQueue::new(core_workers, max_workers, spawner);
        workers.start_core(core_workers)?;
        Ok(Arc::new(Self {
            capacity: CapacityBroker::new(capacity),
            workers,
        }))
    }

    /// One ownership unit bound to this executor after cancellation-safe waiting.
    ///
    /// # Errors
    ///
    /// - [`DropCapacityError`] when all effective ownership capacity has been retired;
    /// - [`DropCapacityError`] when the bounded ownership waiter queue is full;
    /// - [`DropCapacityError`] when the capacity broker closes before granting the request.
    pub(super) async fn reserve(self: &Arc<Self>) -> Result<DropReservation, DropCapacityError> {
        let permit = self.capacity.acquire_one().await?;
        Ok(DropReservation::new(Arc::clone(self), permit))
    }

    /// Binds one immediately available unit to this executor.
    ///
    /// # Errors
    ///
    /// - [`DropCapacityError`] when one ownership unit cannot be granted immediately.
    pub(super) fn try_reserve(self: &Arc<Self>) -> Result<DropReservation, DropCapacityError> {
        let permit = self.capacity.try_acquire(1)?;
        Ok(DropReservation::new(Arc::clone(self), permit))
    }

    /// Independent reservations for one complete immediately available batch.
    ///
    /// # Errors
    ///
    /// - [`DropCapacityError`] when the complete non-empty batch cannot be granted atomically.
    pub(super) fn try_reserve_many(
        self: &Arc<Self>,
        count: usize,
    ) -> Result<Vec<DropReservation>, DropCapacityError> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let mut combined = self.capacity.try_acquire(count)?;
        Ok(self.split_reservations(&mut combined, count))
    }

    /// Gives each accepted lifetime an independent one-unit reservation.
    fn split_reservations(
        self: &Arc<Self>,
        combined: &mut OwnershipPermit,
        count: usize,
    ) -> Vec<DropReservation> {
        let mut reservations = Vec::with_capacity(count);
        for _ in 0..count {
            let permit = combined
                .split_one()
                .expect("the atomic reservation contains the requested permits");
            reservations.push(DropReservation::new(Arc::clone(self), permit));
        }
        reservations
    }

    /// Charged batch queued for isolated destruction.
    ///
    /// If the worker queue is closed, this closes capacity admission and retains the batch permanently.
    pub(super) fn submit(&self, batch: DropBatch) {
        if let Err(batch) = self.workers.submit(batch) {
            self.capacity.close();
            std::mem::forget(batch);
        }
    }

    /// Point-in-time ownership-admission and cleanup-worker state.
    pub(super) fn snapshot(&self) -> ExecutorSnapshot {
        ExecutorSnapshot {
            capacity: self.capacity.snapshot(),
            cleanup: self.workers.snapshot(),
        }
    }
}

impl Drop for DropExecutor {
    /// Closes capacity admission before asking cleanup workers to exit.
    fn drop(&mut self) {
        self.capacity.close();
        self.workers.close();
    }
}
