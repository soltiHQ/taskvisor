//! Prevents management commands from committing after shutdown closes admission.
//!
//! Every mutating management path uses this gate for its last shutdown check.
//! The shutdown workflow takes the same gate, closes admission, and then asks
//! the registry for an ordering fence. Commands already committed are ahead of
//! that fence. A queue reservation by itself is not a commit.

use std::sync::atomic::Ordering;

use tokio::sync::mpsc;

use super::super::SupervisorCore;
use crate::{core::registry::RegistryCommand, error::RuntimeError};

impl SupervisorCore {
    /// Closes command admission after in-progress commits leave the gate.
    pub(in crate::core::runtime) fn mark_shutting_down(&self) {
        let _gate = self
            .admission_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.shutting_down.store(true, Ordering::Release);
    }

    /// Exposes admission closure to cross-component race tests.
    #[cfg(all(test, feature = "controller"))]
    pub(crate) fn close_registry_admission_for_test(&self) {
        self.mark_shutting_down();
    }

    /// Holds shutdown ordering across a command's final check and queue commit.
    ///
    /// Returns `None` after shutdown has closed admission.
    pub(super) fn command_admission(&self) -> Option<std::sync::MutexGuard<'_, ()>> {
        let gate = self
            .admission_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.is_shutting_down() {
            None
        } else {
            Some(gate)
        }
    }

    /// Acquires immediate queue capacity and the final shutdown gate together.
    pub(super) fn try_reserve_command_admission(
        &self,
    ) -> Result<
        (
            mpsc::Permit<'_, RegistryCommand>,
            std::sync::MutexGuard<'_, ()>,
        ),
        RuntimeError,
    > {
        if self.is_shutting_down() {
            return Err(RuntimeError::ShuttingDown);
        }
        let permit = self.cmd_tx.try_reserve().map_err(|error| match error {
            mpsc::error::TrySendError::Full(()) => RuntimeError::CommandQueueFull,
            mpsc::error::TrySendError::Closed(()) => RuntimeError::ShuttingDown,
        })?;
        let Some(admission) = self.command_admission() else {
            drop(permit);
            return Err(RuntimeError::ShuttingDown);
        };
        Ok((permit, admission))
    }

    /// Closes admission and waits until the registry reaches every prior commit.
    ///
    /// Backpressured operations must pass the gate again after they receive capacity.
    pub(in crate::core::runtime) async fn close_admission_and_fence_registry(
        &self,
    ) -> Result<(), RuntimeError> {
        self.mark_shutting_down();
        self.registry.fence().await
    }
}
