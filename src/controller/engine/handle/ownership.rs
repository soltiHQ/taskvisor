//! Reserves supervisor-owned cleanup capacity for submissions.
//!
//! Each [`ControllerSpec`] receives a cleanup reservation before command intake.
//! Waiting stops if the controller channel closes.
//! Capacity, timeout, and cleanup-worker failures remain pre-intake controller errors.

use std::{future::Future, time::Duration};

use crate::{
    controller::{ControllerError, ControllerSpec},
    core::deferred_drop::{
        DropAdmissionError, DropCapacityError, DropReservation, DropStartError, OWNERSHIP_RESOURCE,
        OwnedTask,
    },
    events::Event,
};

use super::ControllerHandle;

impl ControllerHandle {
    /// Cleanup-ownership reservation canceled when controller intake closes.
    async fn reserve_or_closed(
        &self,
        reservation: impl Future<Output = Result<DropReservation, DropAdmissionError>>,
    ) -> Result<DropReservation, ControllerError> {
        tokio::pin!(reservation);
        tokio::select! {
            biased;
            _ = self.tx.closed() => Err(ControllerError::Closed),
            result = &mut reservation => result.map_err(Self::admission_error),
        }
    }

    /// Ownership-only deadline with controller closure taking precedence.
    async fn reserve_or_closed_with_timeout(
        &self,
        reservation: impl Future<Output = Result<DropReservation, DropAdmissionError>>,
        wait_for: Duration,
    ) -> Result<DropReservation, ControllerError> {
        tokio::pin!(reservation);
        tokio::select! {
            biased;
            _ = self.tx.closed() => Err(ControllerError::Closed),
            result = &mut reservation => result.map_err(Self::admission_error),
            _ = tokio::time::sleep(wait_for) => {
                Err(ControllerError::OwnershipAdmissionTimeout { timeout: wait_for })
            }
        }
    }

    /// Waits for deterministic test capacity or controller closure.
    #[cfg(test)]
    async fn reserve_capacity_or_closed(
        &self,
        reservation: impl Future<Output = Result<DropReservation, DropCapacityError>>,
    ) -> Result<DropReservation, ControllerError> {
        tokio::pin!(reservation);
        tokio::select! {
            biased;
            _ = self.tx.closed() => Err(ControllerError::Closed),
            result = &mut reservation => result.map_err(Self::capacity_error),
        }
    }

    /// Test-source counterpart to [`Self::reserve_or_closed_with_timeout`].
    #[cfg(test)]
    async fn reserve_capacity_or_closed_with_timeout(
        &self,
        reservation: impl Future<Output = Result<DropReservation, DropCapacityError>>,
        wait_for: Duration,
    ) -> Result<DropReservation, ControllerError> {
        tokio::pin!(reservation);
        tokio::select! {
            biased;
            _ = self.tx.closed() => Err(ControllerError::Closed),
            result = &mut reservation => result.map_err(Self::capacity_error),
            _ = tokio::time::sleep(wait_for) => {
                Err(ControllerError::OwnershipAdmissionTimeout { timeout: wait_for })
            }
        }
    }

    /// Converts bounded exhaustion or closed cleanup admission into a controller error.
    fn capacity_error(error: DropCapacityError) -> ControllerError {
        match error.limit() {
            Some(limit) => ControllerError::ResourceLimit {
                resource: OWNERSHIP_RESOURCE,
                limit: limit.get(),
            },
            None => ControllerError::Closed,
        }
    }

    /// Converts cleanup worker startup failure into a controller error.
    fn start_error(error: DropStartError) -> ControllerError {
        ControllerError::ThreadStartFailed {
            component: "destructor_isolation",
            worker: error.worker(),
            kind: error.source_kind(),
            raw_os_error: error.raw_os_error(),
        }
    }

    /// Converts ownership admission failure into a controller error.
    fn admission_error(error: DropAdmissionError) -> ControllerError {
        match error {
            DropAdmissionError::Start(error) => Self::start_error(error),
            DropAdmissionError::Capacity(error) => Self::capacity_error(error),
        }
    }

    /// Reserves destructor ownership before command intake.
    pub(super) async fn own(
        &self,
        spec: ControllerSpec,
    ) -> Result<OwnedTask<ControllerSpec>, ControllerError> {
        #[cfg(test)]
        let reservation = match &self.reservation_source {
            Some(source) => self.reserve_capacity_or_closed(source.reserve()).await?,
            None => self.reserve_or_closed(self.drop_domain.reserve()).await?,
        };
        #[cfg(not(test))]
        let reservation = self.reserve_or_closed(self.drop_domain.reserve()).await?;

        Ok(self.attach_ownership(spec, reservation))
    }

    /// Reserves destructor ownership up to a caller-provided deadline.
    pub(super) async fn own_with_ownership_timeout(
        &self,
        spec: ControllerSpec,
        wait_for: Duration,
    ) -> Result<OwnedTask<ControllerSpec>, ControllerError> {
        #[cfg(test)]
        let reservation = match &self.reservation_source {
            Some(source) => {
                self.reserve_capacity_or_closed_with_timeout(source.reserve(), wait_for)
                    .await?
            }
            None => {
                self.reserve_or_closed_with_timeout(self.drop_domain.reserve(), wait_for)
                    .await?
            }
        };
        #[cfg(not(test))]
        let reservation = self
            .reserve_or_closed_with_timeout(self.drop_domain.reserve(), wait_for)
            .await?;

        Ok(self.attach_ownership(spec, reservation))
    }

    /// Reserves destructor ownership without waiting.
    pub(super) fn try_own(
        &self,
        spec: ControllerSpec,
    ) -> Result<OwnedTask<ControllerSpec>, ControllerError> {
        #[cfg(test)]
        let reservation = match &self.reservation_source {
            Some(source) => source.try_reserve().map_err(Self::capacity_error),
            None => self
                .drop_domain
                .try_reserve()
                .map_err(Self::admission_error),
        };
        #[cfg(not(test))]
        let reservation = self
            .drop_domain
            .try_reserve()
            .map_err(Self::admission_error);

        Ok(self.attach_ownership(spec, reservation?))
    }

    /// Task ownership coupled to its cleanup reservation and panic diagnostic.
    fn attach_ownership(
        &self,
        spec: ControllerSpec,
        reservation: DropReservation,
    ) -> OwnedTask<ControllerSpec> {
        let retained = spec.task_spec().task().clone();
        let mut owned = OwnedTask::new(spec, retained, reservation);
        let bus = self.bus.clone();
        owned.cleanup.set_panic_reporter(move |message| {
            bus.publish_lazy(|| {
                Event::runtime_failure("controller", format!("task_drop_panicked: {message}"))
            });
        });
        owned
    }
}
