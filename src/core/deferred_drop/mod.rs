//! Runs user-owned destructors outside Taskvisor's async runtime paths.
//!
//! `SupervisorBuilder` creates one [`DropDomain`] for each supervisor. Runtime task admission, controller submission,
//! and subscriber construction reserve this domain before Taskvisor accepts their user-owned values.
//! Controller, registry, subscriber, and reaper paths carry its bundle until their final retained
//! values are ready for destruction.
//!
//! ```text
//! runtime, controller, or builder
//!               │ accepted user value
//!               ▼
//!           DropDomain ──► DropReservation
//!                               ▼
//!                       retained user value
//!                               ▼
//!                         DropBundle
//!                               ▼
//!                       internal ownership
//!                               ▼
//!                    cleanup executor worker
//! ```
//!
//! This isolation prevents a blocking or panicking destructor from running on a Tokio worker, the registry listener,
//! or a controller loop. Cleanup runs on dedicated operating-system threads after internal ownership ends.
//! User destructors do not run under the bundle, capacity, or worker-queue locks.
//! Clean cleanup returns the reserved unit. Poisoned cleanup or a destructor panic retires that unit permanently.
//! Values that never cross an ownership hand-off remain caller-owned and do not enter these workers.

/// Public error label for the supervisor-local ownership budget.
pub(crate) const OWNERSHIP_RESOURCE: &str = "owned_user_lifetimes";

mod bundle;
mod capacity;
mod domain;
mod error;
mod executor;

pub(crate) use bundle::{DropBundle, DropReservation, OwnedTask};
pub(crate) use domain::DropDomain;
pub(crate) use error::{DropAdmissionError, DropCapacityError, DropStartError};

#[cfg(test)]
mod test_support;
#[cfg(test)]
use executor::{
    CORE_WORKER_COUNT, DropExecutor, ELASTIC_IDLE_TIMEOUT, MAX_WORKER_COUNT, WorkerSpawner,
    system_spawner, worker_loop,
};
#[cfg(test)]
pub(crate) use test_support::{
    TestLazyDomain, TestReservationSource, isolated_test_reservation, test_reservation,
};

#[cfg(test)]
mod tests;
