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
    sync::{Arc, Mutex},
};

use super::{
    bundle::DropReservation,
    error::{DropAdmissionError, DropCapacityError, DropStartError},
    executor::{CORE_WORKER_COUNT, DropExecutor, WorkerSpawner, system_spawner},
};

/// Configuration and startup state shared by every domain handle.
struct DropDomainInner {
    /// Requested persistent worker count before the capacity ceiling.
    worker_count: usize,
    /// Maximum accepted user lifetimes charged at one time.
    capacity: usize,
    /// Thread factory used for core and elastic workers.
    spawner: Arc<WorkerSpawner>,
    /// Executor published only after transactional startup succeeds.
    executor: Mutex<Option<Arc<DropExecutor>>>,
}

/// Cloneable handle to one supervisor-local cleanup budget.
#[derive(Clone)]
pub(crate) struct DropDomain(
    /// Shared configuration, startup gate, and published executor.
    Arc<DropDomainInner>,
);

impl DropDomain {
    /// Creates the production domain without starting worker threads.
    ///
    /// # Panics
    ///
    /// Panics when `capacity` is zero.
    pub(crate) fn unstarted(capacity: usize) -> Self {
        Self::unstarted_with(CORE_WORKER_COUNT, capacity, system_spawner())
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
    pub(super) fn unstarted_with(
        worker_count: usize,
        capacity: usize,
        spawner: Arc<WorkerSpawner>,
    ) -> Result<Self, DropStartError> {
        if worker_count == 0 || capacity == 0 {
            return Err(DropStartError::new(
                0,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "worker count and capacity must be positive",
                ),
            ));
        }
        Ok(Self(Arc::new(DropDomainInner {
            worker_count,
            capacity,
            spawner,
            executor: Mutex::new(None),
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
        *executor = Some(Arc::clone(&started));
        Ok(started)
    }

    /// Returns the supervisor-local ownership limit.
    pub(crate) fn capacity(&self) -> usize {
        self.0.capacity
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
        if count > self.0.capacity {
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
