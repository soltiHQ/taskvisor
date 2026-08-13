//! Reserves supervisor-owned cleanup capacity for submissions.
//!
//! Submission methods wrap each [`ControllerSpec`] in an `OwnedTask` before command intake.
//! Waiting stops if the controller channel closes.
//! Fail-fast reservation converts capacity and worker-start failures into controller errors.

use std::future::Future;

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
    /// Waits for ownership or reports that controller intake has closed.
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

    /// Converts ownership capacity exhaustion into a controller error.
    fn capacity_error(error: DropCapacityError) -> ControllerError {
        ControllerError::ResourceLimit {
            resource: OWNERSHIP_RESOURCE,
            limit: error.limit(),
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

        let retained = spec.task_spec().task().clone();
        let mut owned = OwnedTask::new(spec, retained, reservation);
        let bus = self.bus.clone();
        owned.cleanup.set_panic_reporter(move |message| {
            bus.publish_lazy(|| {
                Event::runtime_failure("controller", format!("task_drop_panicked: {message}"))
            });
        });
        Ok(owned)
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

        let retained = spec.task_spec().task().clone();
        let mut owned = OwnedTask::new(spec, retained, reservation?);
        let bus = self.bus.clone();
        owned.cleanup.set_panic_reporter(move |message| {
            bus.publish_lazy(|| {
                Event::runtime_failure("controller", format!("task_drop_panicked: {message}"))
            });
        });
        Ok(owned)
    }
}
