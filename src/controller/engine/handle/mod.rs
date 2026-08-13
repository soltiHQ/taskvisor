//! Owns the command-side boundary of the controller engine.
//!
//! Submission methods first reserve safe cleanup ownership for the user task.
//! They then send the task through the same ordered channel used by task-ID
//! remove and cancel operations.
//!
//! ```text
//! caller
//!      ├── submission ──► cleanup ownership ──► ordered command queue
//!      └── identity operation ──► ordered command queue
//! ```
//!
//! The shared queue preserves intake order between submissions and identity
//! commands. The lifecycle driver owns controller-side processing and state
//! transitions after intake. Registry replies and physical completion remain
//! authoritative for runtime results.

use tokio::sync::mpsc;

use crate::{core::deferred_drop::DropDomain, events::Bus};

#[cfg(test)]
use crate::core::deferred_drop::TestReservationSource;

use super::ControllerCommand;

mod identity;
mod ownership;
mod submission;

/// Cloneable client for the internal controller command queue.
#[derive(Clone)]
pub(crate) struct ControllerHandle {
    /// Ordered controller command sender.
    tx: mpsc::Sender<ControllerCommand>,
    /// Event bus used by ownership cleanup diagnostics.
    bus: Bus,
    /// Supervisor-local user-ownership domain.
    drop_domain: DropDomain,
    /// Test-only source for deterministic ownership capacity.
    #[cfg(test)]
    reservation_source: Option<TestReservationSource>,
}

impl ControllerHandle {
    /// Creates a handle for one controller engine.
    pub(super) fn new(
        tx: mpsc::Sender<ControllerCommand>,
        bus: Bus,
        drop_domain: DropDomain,
    ) -> Self {
        Self {
            tx,
            bus,
            drop_domain,
            #[cfg(test)]
            reservation_source: None,
        }
    }

    /// Uses a deterministic ownership source in tests.
    #[cfg(test)]
    pub(super) fn with_reservation_source(mut self, source: TestReservationSource) -> Self {
        self.reservation_source = Some(source);
        self
    }
}
