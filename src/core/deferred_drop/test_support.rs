//! Builds isolated cleanup domains for cross-module tests.

use std::{
    io,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering},
    },
};

use super::{
    bundle::DropReservation,
    domain::DropDomain,
    error::{DropAdmissionError, DropCapacityError},
    executor::{CORE_WORKER_COUNT, WorkerSpawner, system_spawner, worker_loop},
};

/// Returns one unit from the process-wide shared test domain.
///
/// # Panics
///
/// Panics when the test worker set cannot start or no ownership slot is immediately available.
pub(crate) fn test_reservation() -> DropReservation {
    static TEST_DOMAIN: OnceLock<DropDomain> = OnceLock::new();
    TEST_DOMAIN
        .get_or_init(|| {
            DropDomain::try_start_with(2, 16_384, system_spawner())
                .expect("the shared test destructor domain must start")
        })
        .try_reserve()
        .expect("the shared test destructor executor has sufficient ownership slots")
}

/// Starts a fresh one-unit domain and returns its reservation.
///
/// # Panics
///
/// Panics when the isolated worker cannot start or its ownership slot cannot be reserved.
pub(crate) fn isolated_test_reservation() -> DropReservation {
    DropDomain::try_start_with(1, 1, system_spawner())
        .expect("the isolated destructor domain must start")
        .try_reserve()
        .expect("a fresh isolated test executor has one ownership slot")
}

/// Eager isolated domain used by deterministic saturation tests.
#[derive(Clone)]
pub(crate) struct TestReservationSource(
    /// Started domain shared by clones of this source.
    DropDomain,
);

impl TestReservationSource {
    /// Starts one worker with the requested ownership capacity.
    ///
    /// # Panics
    ///
    /// Panics when `capacity` is zero or the isolated worker cannot start.
    pub(crate) fn new(capacity: usize) -> Self {
        Self(
            DropDomain::try_start_with(1, capacity, system_spawner())
                .expect("the isolated test destructor domain must start"),
        )
    }

    /// Waits for one unit in the already-started test domain.
    ///
    /// # Errors
    ///
    /// Returns a capacity error when admission closes or the unit cannot be granted.
    pub(crate) async fn reserve(&self) -> Result<DropReservation, DropCapacityError> {
        match self.0.reserve().await {
            Ok(reservation) => Ok(reservation),
            Err(DropAdmissionError::Capacity(error)) => Err(error),
            Err(DropAdmissionError::Start(_)) => {
                unreachable!("the test reservation source starts eagerly")
            }
        }
    }

    /// Requests a complete test batch without partial admission.
    ///
    /// # Errors
    ///
    /// Returns a capacity error when the complete batch exceeds the limit or is unavailable.
    pub(crate) fn try_reserve_many(
        &self,
        count: usize,
    ) -> Result<Vec<DropReservation>, DropCapacityError> {
        match self.0.try_reserve_many(count) {
            Ok(reservations) => Ok(reservations),
            Err(DropAdmissionError::Capacity(error)) => Err(error),
            Err(DropAdmissionError::Start(_)) => {
                unreachable!("the test reservation source starts eagerly")
            }
        }
    }

    /// Requests one immediately available test unit.
    ///
    /// # Errors
    ///
    /// Returns a capacity error when no ownership slot is available.
    pub(crate) fn try_reserve(&self) -> Result<DropReservation, DropCapacityError> {
        match self.0.try_reserve() {
            Ok(reservation) => Ok(reservation),
            Err(DropAdmissionError::Capacity(error)) => Err(error),
            Err(DropAdmissionError::Start(_)) => {
                unreachable!("the test reservation source starts eagerly")
            }
        }
    }

    /// Shares this source's domain with the code under test.
    pub(crate) fn domain(&self) -> DropDomain {
        self.0.clone()
    }
}

/// Dormant domain with a one-shot startup failure at a chosen worker.
pub(crate) struct TestLazyDomain {
    /// Domain passed to the production admission path.
    domain: DropDomain,
    /// Calls observed by the injected worker factory.
    spawn_calls: Arc<AtomicUsize>,
}

impl TestLazyDomain {
    /// Injects one failure when startup reaches the selected worker index.
    ///
    /// # Panics
    ///
    /// Panics when `capacity` is zero.
    pub(crate) fn fail_first_start_at_worker(capacity: usize, worker: usize) -> Self {
        let spawn_calls = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicBool::new(false));
        let spawn_calls_for_spawner = Arc::clone(&spawn_calls);
        let failed_for_spawner = Arc::clone(&failed);
        let spawner: Arc<WorkerSpawner> = Arc::new(move |index, launcher| {
            spawn_calls_for_spawner.fetch_add(1, AtomicOrdering::AcqRel);
            if index == worker && !failed_for_spawner.swap(true, AtomicOrdering::AcqRel) {
                return Err(io::Error::other("injected lazy core startup failure"));
            }
            std::thread::Builder::new()
                .name(format!("taskvisor-test-drop-{index}"))
                .spawn(move || {
                    if let Ok(launch) = launcher.recv() {
                        worker_loop(launch);
                    }
                })
        });
        let domain = DropDomain::unstarted_with(CORE_WORKER_COUNT, capacity, spawner)
            .expect("the injected lazy domain configuration must be valid");
        Self {
            domain,
            spawn_calls,
        }
    }

    /// Shares the lazy domain with the code under test.
    pub(crate) fn domain(&self) -> DropDomain {
        self.domain.clone()
    }

    /// Reports calls made to the injected worker factory.
    pub(crate) fn spawn_calls(&self) -> usize {
        self.spawn_calls.load(AtomicOrdering::Acquire)
    }
}
