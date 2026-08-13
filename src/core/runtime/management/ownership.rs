//! Reserves cleanup capacity before the runtime accepts a user-owned task.
//!
//! Direct adds and static batches use this layer before registry handoff.
//! A reservation is attached to the [`TaskSpec`] inside an [`OwnedTask`].
//! It then follows that task through registry membership, actor execution,
//! physical reaping, and final destruction.
//!
//! Waiting admission observes shutdown. Static batches use one immediate
//! atomic reservation for the full batch.
//! Admission failures are translated into the runtime's thread-start or resource-limit errors here.

use std::{future::Future, sync::Arc};

use super::super::SupervisorCore;
use crate::{
    core::deferred_drop::{self, OwnedTask},
    error::RuntimeError,
    events::Event,
    tasks::TaskSpec,
};

impl SupervisorCore {
    /// Waits for one slot in this supervisor's cleanup ownership domain.
    pub(super) async fn reserve_ownership(
        &self,
    ) -> Result<deferred_drop::DropReservation, deferred_drop::DropAdmissionError> {
        #[cfg(test)]
        let source = {
            self.ownership_source
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        };
        #[cfg(test)]
        if let Some(source) = source {
            return source
                .reserve()
                .await
                .map_err(deferred_drop::DropAdmissionError::Capacity);
        }
        self.drop_domain.reserve().await
    }

    /// Reserves a complete static batch without entering the waiter queue.
    pub(in crate::core::runtime) fn try_reserve_ownership_many(
        &self,
        count: usize,
    ) -> Result<Vec<deferred_drop::DropReservation>, deferred_drop::DropAdmissionError> {
        #[cfg(test)]
        let source = {
            self.ownership_source
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        };
        #[cfg(test)]
        if let Some(source) = source {
            return source
                .try_reserve_many(count)
                .map_err(deferred_drop::DropAdmissionError::Capacity);
        }
        self.drop_domain.try_reserve_many(count)
    }

    /// Preserves cleanup admission failure details in the runtime error model.
    pub(in crate::core::runtime) fn ownership_admission_error(
        error: deferred_drop::DropAdmissionError,
    ) -> RuntimeError {
        match error {
            deferred_drop::DropAdmissionError::Start(error) => {
                let kind = error.source_kind();
                RuntimeError::ThreadStartFailed {
                    component: "destructor_isolation",
                    source: std::io::Error::new(kind, error),
                }
            }
            deferred_drop::DropAdmissionError::Capacity(error) => {
                RuntimeError::ResourceLimitReached {
                    resource: deferred_drop::OWNERSHIP_RESOURCE,
                    limit: error.limit(),
                }
            }
        }
    }

    /// Replaces the reservation source for deterministic admission tests.
    #[cfg(test)]
    pub(in crate::core::runtime) fn set_ownership_source_for_test(
        &self,
        source: deferred_drop::TestReservationSource,
    ) {
        *self
            .ownership_source
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(source);
    }

    /// Cancels an ownership wait when shutdown starts or the command queue closes.
    pub(super) async fn wait_for_ownership<T, F>(&self, reserve: F) -> Result<T, RuntimeError>
    where
        F: Future<Output = Result<T, deferred_drop::DropAdmissionError>>,
    {
        tokio::select! {
            biased;
            _ = self.shutdown.started.cancelled() => Err(RuntimeError::ShuttingDown),
            _ = self.cmd_tx.closed() => Err(RuntimeError::ShuttingDown),
            reservation = reserve => reservation.map_err(Self::ownership_admission_error),
        }
    }

    /// Attaches cleanup ownership and destructor-panic reporting to a task specification.
    pub(in crate::core::runtime) fn own_task(
        &self,
        spec: TaskSpec,
        reservation: deferred_drop::DropReservation,
    ) -> OwnedTask<TaskSpec> {
        let retained = Arc::clone(spec.task());
        let mut owned = OwnedTask::new(spec, retained, reservation);
        let bus = self.bus.clone();
        owned.cleanup.set_panic_reporter(move |message| {
            bus.publish_lazy(|| {
                Event::runtime_failure("task_destructor", format!("task_drop_panicked: {message}"))
            });
        });
        owned
    }
}
