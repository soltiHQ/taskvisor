//! Runs isolated user destructors on supervisor-local worker threads.
//!
//! [`DropExecutor`] connects the ownership [`CapacityBroker`] to a [`WorkerQueue`].
//! Reservations keep the executor alive. It routes submitted [`DropBatch`] values from
//! controller, registry, subscriber, and force-abort terminal paths to the worker queue.
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
    /// Starts the configured core set up to the domain's worker ceiling.
    ///
    /// # Errors
    ///
    /// Returns an error when any required worker cannot be created or report ready.
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

    /// Waits for one unit and binds it to this executor.
    ///
    /// # Errors
    ///
    /// Returns an error when admission closes or the unit can no longer be granted.
    pub(super) async fn reserve(self: &Arc<Self>) -> Result<DropReservation, DropCapacityError> {
        let permit = self.capacity.acquire(1).await?;
        Ok(DropReservation::new(Arc::clone(self), permit))
    }

    /// Binds one immediately available unit to this executor.
    ///
    /// # Errors
    ///
    /// Returns an error when admission is closed or the unit is not immediately available.
    pub(super) fn try_reserve(self: &Arc<Self>) -> Result<DropReservation, DropCapacityError> {
        let permit = self.capacity.try_acquire(1)?;
        Ok(DropReservation::new(Arc::clone(self), permit))
    }

    /// Waits for a complete test batch and creates one reservation per unit.
    ///
    /// # Errors
    ///
    /// Returns an error when the batch is invalid, admission closes, or the complete batch can no longer be granted.
    #[cfg(test)]
    pub(super) async fn reserve_many(
        self: &Arc<Self>,
        count: usize,
    ) -> Result<Vec<DropReservation>, DropCapacityError> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let mut combined = self.capacity.acquire(count).await?;
        Ok(self.split_reservations(&mut combined, count))
    }

    /// Creates one reservation per unit only when the complete batch is available.
    ///
    /// # Errors
    ///
    /// Returns an error when the batch is invalid, admission is closed, or the complete batch is not immediately available.
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

    /// Enqueues one charged batch for isolated destruction.
    ///
    /// If the worker queue is closed, this closes capacity admission and retains the batch permanently.
    pub(super) fn submit(&self, batch: DropBatch) {
        if let Err(batch) = self.workers.submit(batch) {
            self.capacity.close();
            std::mem::forget(batch);
        }
    }

    /// Copies ownership-admission and cleanup-worker state.
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
