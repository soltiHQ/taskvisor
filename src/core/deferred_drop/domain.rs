//! Owns one supervisor's cleanup capacity and starts cleanup workers lazily.
//!
//! `SupervisorBuilder` creates a [`DropDomain`] and stores it in `SupervisorCore`. Subscriber construction
//! reserves a complete batch from the domain. Runtime and controller admission reserve one unit per accepted task.
//!
//! The first valid, non-empty reservation attempts to start the cleanup executor.
//! Startup is transactional. No executor is published unless every required core worker reports ready.
//! Clones share this gate and the same capacity budget.

use std::{
    io,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};

use crate::core::OwnershipSnapshot;

use super::{
    bundle::DropReservation,
    capacity::{CapacityRetirement, RetirementReporter},
    error::{DropAdmissionError, DropCapacityError, DropStartError},
    executor::{CORE_WORKER_COUNT, DropExecutor, WorkerSpawner, system_spawner},
};

/// Configuration and startup state shared by every domain handle.
struct DropDomainInner {
    /// Requested persistent worker count before the capacity ceiling.
    worker_count: usize,
    /// Maximum accepted user lifetimes, or `None` for unlimited admission.
    capacity: Option<NonZeroUsize>,
    /// Thread factory used for core and elastic workers.
    spawner: Arc<WorkerSpawner>,
    /// Executor published only after transactional startup succeeds.
    executor: Mutex<Option<Arc<DropExecutor>>>,
    /// Callback installed on the current or next executor capacity broker.
    retirement_reporter: Mutex<Option<RetirementReporter>>,
}

/// Cloneable handle to one supervisor-local cleanup budget.
#[derive(Clone)]
pub(crate) struct DropDomain(
    /// Shared configuration, startup gate, and published executor.
    Arc<DropDomainInner>,
);

impl DropDomain {
    /// Creates the production domain without starting worker threads.
    pub(crate) fn unstarted(capacity: Option<NonZeroUsize>) -> Self {
        Self::unstarted_with_limit(CORE_WORKER_COUNT, capacity, system_spawner())
            .expect("the production drop domain configuration must be valid")
    }

    /// Starts a test domain with the production worker policy.
    ///
    /// # Errors
    ///
    /// Returns an error when the required core worker set cannot start.
    #[cfg(test)]
    pub(crate) fn try_start(capacity: usize) -> Result<Self, DropStartError> {
        Self::try_start_with(CORE_WORKER_COUNT, capacity, system_spawner())
    }

    /// Starts a test domain through an injected worker factory.
    ///
    /// # Errors
    ///
    /// Returns an error for zero limits or an incomplete core start.
    #[cfg(test)]
    pub(super) fn try_start_with(
        worker_count: usize,
        capacity: usize,
        spawner: Arc<WorkerSpawner>,
    ) -> Result<Self, DropStartError> {
        let domain = Self::unstarted_with(worker_count, capacity, spawner)?;
        domain.executor()?;
        Ok(domain)
    }

    /// Stores explicit worker policy without starting the executor.
    ///
    /// # Errors
    ///
    /// Returns an error when the worker count or capacity is zero.
    #[cfg(test)]
    pub(super) fn unstarted_with(
        worker_count: usize,
        capacity: usize,
        spawner: Arc<WorkerSpawner>,
    ) -> Result<Self, DropStartError> {
        let capacity = NonZeroUsize::new(capacity).ok_or_else(|| {
            DropStartError::new(
                0,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "worker count and capacity must be positive",
                ),
            )
        })?;
        Self::unstarted_with_limit(worker_count, Some(capacity), spawner)
    }

    /// Stores finite or unlimited worker policy without starting the executor.
    fn unstarted_with_limit(
        worker_count: usize,
        capacity: Option<NonZeroUsize>,
        spawner: Arc<WorkerSpawner>,
    ) -> Result<Self, DropStartError> {
        if worker_count == 0 {
            return Err(DropStartError::new(
                0,
                io::Error::new(io::ErrorKind::InvalidInput, "worker count must be positive"),
            ));
        }
        Ok(Self(Arc::new(DropDomainInner {
            worker_count,
            capacity,
            spawner,
            executor: Mutex::new(None),
            retirement_reporter: Mutex::new(None),
        })))
    }

    /// Returns the published executor or performs one transactional startup.
    ///
    /// # Errors
    ///
    /// Returns an error when a required core worker cannot be created or report ready.
    fn executor(&self) -> Result<Arc<DropExecutor>, DropStartError> {
        let mut executor = self
            .0
            .executor
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(executor) = executor.as_ref() {
            return Ok(Arc::clone(executor));
        }

        let started = DropExecutor::try_start_with(
            self.0.worker_count,
            self.0.capacity,
            Arc::clone(&self.0.spawner),
        )?;
        if let Some(reporter) = self
            .0
            .retirement_reporter
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
        {
            started.capacity.set_retirement_reporter(reporter);
        }
        *executor = Some(Arc::clone(&started));
        Ok(started)
    }

    /// Returns the supervisor-local ownership limit.
    pub(crate) fn capacity(&self) -> Option<NonZeroUsize> {
        self.0.capacity
    }

    /// Installs the best-effort callback for committed finite-capacity retirement.
    pub(crate) fn set_retirement_reporter<F>(&self, report: F)
    where
        F: Fn(usize, usize, usize) + Send + Sync + 'static,
    {
        let reporter: RetirementReporter = Arc::new(move |retirement: CapacityRetirement| {
            report(
                retirement.configured_capacity,
                retirement.effective_capacity,
                retirement.retired_units,
            );
        });
        *self
            .0
            .retirement_reporter
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(Arc::clone(&reporter));

        let executor = self
            .0
            .executor
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(Arc::clone);
        if let Some(executor) = executor {
            executor.capacity.set_retirement_reporter(reporter);
        }
    }

    /// Returns current ownership and deferred-cleanup state without starting workers.
    pub(crate) fn snapshot(&self, runtime_open: bool) -> OwnershipSnapshot {
        let executor = self
            .0
            .executor
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(Arc::clone);

        let Some(executor) = executor else {
            let configured = self.0.capacity.map(NonZeroUsize::get);
            return OwnershipSnapshot::new(
                configured,
                configured,
                configured,
                0,
                runtime_open,
                0,
                0,
            );
        };

        let snapshot = executor.snapshot();
        OwnershipSnapshot::new(
            snapshot.capacity.configured_limit,
            snapshot.capacity.effective_limit,
            snapshot.capacity.available,
            snapshot.capacity.waiters,
            runtime_open && snapshot.capacity.open,
            snapshot.cleanup.queued,
            snapshot.cleanup.running,
        )
    }

    /// Starts the domain when needed and waits for one ownership unit.
    ///
    /// # Errors
    ///
    /// Returns a startup error or a capacity error from this domain.
    pub(crate) async fn reserve(&self) -> Result<DropReservation, DropAdmissionError> {
        self.executor()
            .map_err(DropAdmissionError::Start)?
            .reserve()
            .await
            .map_err(DropAdmissionError::Capacity)
    }

    /// Starts the domain when needed and requests one unit without waiting.
    ///
    /// # Errors
    ///
    /// Returns a startup error or a capacity error from this domain.
    pub(crate) fn try_reserve(&self) -> Result<DropReservation, DropAdmissionError> {
        self.executor()
            .map_err(DropAdmissionError::Start)?
            .try_reserve()
            .map_err(DropAdmissionError::Capacity)
    }

    /// Requests a complete batch without partial admission or waiting.
    ///
    /// An empty batch returns immediately and does not start the domain.
    ///
    /// # Errors
    ///
    /// Returns a startup error or a capacity error from this domain.
    pub(crate) fn try_reserve_many(
        &self,
        count: usize,
    ) -> Result<Vec<DropReservation>, DropAdmissionError> {
        if count == 0 {
            return Ok(Vec::new());
        }
        if self
            .0
            .capacity
            .is_some_and(|capacity| count > capacity.get())
        {
            return Err(DropAdmissionError::Capacity(DropCapacityError::new(
                self.0.capacity,
            )));
        }
        self.executor()
            .map_err(DropAdmissionError::Start)?
            .try_reserve_many(count)
            .map_err(DropAdmissionError::Capacity)
    }

    /// Reports whether transactional startup has published an executor.
    #[cfg(test)]
    pub(crate) fn is_started(&self) -> bool {
        self.0
            .executor
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some()
    }

    /// Exposes the published executor to white-box tests.
    ///
    /// # Panics
    ///
    /// Panics when the test domain has not started.
    #[cfg(test)]
    pub(super) fn started_executor(&self) -> Arc<DropExecutor> {
        self.0
            .executor
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(Arc::clone)
            .expect("the test domain must already be started")
    }
}
