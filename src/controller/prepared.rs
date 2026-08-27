//! Defines explicit controller submission operations.
//!
//! [`SupervisorHandle::submit`](crate::SupervisorHandle::submit) is the direct entry point to a [`Submit`] operation.
//! [`PreparedSubmission`] provides the same operation with its identity allocated first.
//!
//! ```text
//! ControllerSpec ──► Submit ──► await / execute / try_intake ──► controller intake
//!                         └──► watch ──► TaskWaiter
//! ```
//!
//! Building, configuring, or dropping a submission operation sends no command and starts no work.

use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::time::Duration;

use super::{
    ControllerError, ControllerSpec,
    engine::{Controller, ControllerHandle},
};
use crate::{
    TaskId, TaskWaiter,
    core::{OwnershipTimed, Unwatched, Waiting, Watched},
};

/// An explicit single-use controller submission operation.
///
/// Configure final-outcome delivery and an optional ownership-admission deadline.
/// Finish with `execute` or `try_intake`. The default waiting, unwatched operation can also be awaited directly.
/// APIs that accept a [`Future`] require `execute()` or [`IntoFuture::into_future`].
/// Every terminal method consumes the operation, including when intake returns an error.
///
/// `Watch` and `Admission` are operation type states.
/// [`watch`](Self::watch) and [`ownership_timeout`](Self::ownership_timeout) select them.
/// Applications do not need to name them.
#[must_use = "await the default submission, call and await `execute`, or call `try_intake`"]
pub struct Submit<'a, Watch = Unwatched, Admission = Waiting> {
    /// Direct or pre-identified submission payload retained until a terminal method runs.
    request: SubmitRequest<'a>,

    /// Typed final-outcome selection.
    watch: Watch,

    /// Typed ownership-admission selection.
    _admission: Admission,
}

/// Payload origin kept inline by a [`Submit`] operation.
enum SubmitRequest<'a> {
    /// Direct submission whose identity is allocated only when a terminal method runs.
    Direct {
        /// Missing when this supervisor was built without a controller.
        controller: Option<&'a Controller>,
        /// Specification retained even when the controller is not configured.
        spec: ControllerSpec,
    },

    /// Prepared submission carrying the identity exposed before intake.
    Prepared {
        controller: ControllerHandle,
        id: TaskId,
        spec: ControllerSpec,
    },
}

impl<'a> Submit<'a> {
    /// Lazy direct submission with no identity or controller command.
    ///
    /// The terminal method reports a missing controller as [`ControllerError::NotConfigured`].
    /// This keeps [`SupervisorHandle::submit`](crate::SupervisorHandle::submit) infallible.
    #[inline]
    pub(crate) fn direct(controller: Option<&'a Controller>, spec: ControllerSpec) -> Self {
        Self {
            request: SubmitRequest::Direct { controller, spec },
            watch: Unwatched,
            _admission: Waiting,
        }
    }
}

impl Submit<'static> {
    #[inline]
    fn prepared(controller: ControllerHandle, id: TaskId, spec: ControllerSpec) -> Self {
        Self {
            request: SubmitRequest::Prepared {
                controller,
                id,
                spec,
            },
            watch: Unwatched,
            _admission: Waiting,
        }
    }
}

impl SubmitRequest<'_> {
    /// Terminal resolution of controller configuration and direct identity allocation.
    #[inline(always)]
    fn into_parts(self) -> Result<(ControllerHandle, TaskId, ControllerSpec), ControllerError> {
        match self {
            Self::Direct {
                controller: Some(controller),
                spec,
            } => Ok((controller.handle(), TaskId::next(), spec)),
            Self::Direct {
                controller: None, ..
            } => Err(ControllerError::NotConfigured),
            Self::Prepared {
                controller,
                id,
                spec,
            } => Ok((controller, id, spec)),
        }
    }

    /// Performs fail-fast unwatched intake while keeping direct setup behind one call boundary.
    #[inline(always)]
    fn try_submit(self) -> Result<TaskId, ControllerError> {
        match self {
            Self::Direct {
                controller: Some(controller),
                spec,
            } => try_submit_direct(controller, spec),
            Self::Direct {
                controller: None, ..
            } => Err(ControllerError::NotConfigured),
            Self::Prepared {
                controller,
                id,
                spec,
            } => controller.try_submit_prepared(id, spec),
        }
    }

    /// Performs fail-fast watched intake while keeping direct setup behind one call boundary.
    #[inline(always)]
    fn try_submit_and_watch(
        self,
    ) -> Result<(TaskId, tokio::sync::oneshot::Receiver<crate::TaskOutcome>), ControllerError> {
        match self {
            Self::Direct {
                controller: Some(controller),
                spec,
            } => try_submit_direct_and_watch(controller, spec),
            Self::Direct {
                controller: None, ..
            } => Err(ControllerError::NotConfigured),
            Self::Prepared {
                controller,
                id,
                spec,
            } => controller.try_submit_prepared_and_watch(id, spec),
        }
    }
}

/// Allocates direct-submission state at the terminal call boundary.
fn try_submit_direct(
    controller: &Controller,
    spec: ControllerSpec,
) -> Result<TaskId, ControllerError> {
    controller
        .handle()
        .try_submit_prepared(TaskId::next(), spec)
}

/// Allocates direct watched-submission state at the terminal call boundary.
fn try_submit_direct_and_watch(
    controller: &Controller,
    spec: ControllerSpec,
) -> Result<(TaskId, tokio::sync::oneshot::Receiver<crate::TaskOutcome>), ControllerError> {
    controller
        .handle()
        .try_submit_prepared_and_watch(TaskId::next(), spec)
}

impl<'a, Admission> Submit<'a, Unwatched, Admission> {
    /// Final-outcome delivery through [`TaskWaiter`].
    ///
    /// Successful intake returns only [`TaskWaiter`].
    /// Its [`TaskWaiter::id`] is the submission identity.
    /// The waiter later reports controller rejection or the admitted task's final outcome.
    #[must_use = "configure or execute the watched submission"]
    #[inline]
    pub fn watch(self) -> Submit<'a, Watched, Admission> {
        Submit {
            request: self.request,
            watch: Watched,
            _admission: self._admission,
        }
    }
}

impl<'a, Watch> Submit<'a, Watch, Waiting> {
    /// Cleanup-ownership admission deadline.
    ///
    /// The deadline stops after ownership succeeds.
    /// `execute` can then wait without a deadline for controller command capacity.
    /// An immediately available permit can succeed when `wait_for` is [`Duration::ZERO`].
    /// A timeout sends no command and publishes no lifecycle event.
    #[inline]
    pub fn ownership_timeout(self, wait_for: Duration) -> Submit<'a, Watch, OwnershipTimed> {
        Submit {
            request: self.request,
            watch: self.watch,
            _admission: OwnershipTimed(wait_for),
        }
    }
}

impl Submit<'_, Unwatched, Waiting> {
    /// Waiting controller command intake with an unwatched [`TaskId`] result.
    ///
    /// `Ok(id)` confirms only command intake.
    /// Slot admission and runtime registration happen later.
    /// Awaiting the default operation directly is equivalent to calling this method.
    /// A successful [`PreparedSubmission`] cannot produce `NotConfigured`.
    ///
    /// # Errors
    ///
    /// - Returns [`ControllerError::NotConfigured`] when a direct submission has no controller;
    /// - [`ControllerError::ThreadStartFailed`] when cleanup workers cannot start;
    /// - [`ControllerError::ResourceLimit`] when cleanup ownership is unavailable;
    /// - [`ControllerError::Closed`] when controller intake is closed.
    #[inline]
    pub async fn execute(self) -> Result<TaskId, ControllerError> {
        let (controller, id, spec) = self.request.into_parts()?;
        controller.submit_prepared(id, spec).await
    }

    /// Fail-fast controller command intake with an unwatched [`TaskId`] result.
    ///
    /// `Ok(id)` has the same intake-only meaning as [`execute`](Self::execute).
    /// A successful [`PreparedSubmission`] cannot produce `NotConfigured`.
    ///
    /// # Errors
    ///
    /// - Returns [`ControllerError::NotConfigured`] when a direct submission has no controller;
    /// - [`ControllerError::ThreadStartFailed`] when cleanup workers cannot start;
    /// - [`ControllerError::ResourceLimit`] when cleanup ownership is unavailable;
    /// - [`ControllerError::Full`] when controller command capacity is unavailable;
    /// - [`ControllerError::Closed`] when controller intake is closed.
    #[inline(always)]
    pub fn try_intake(self) -> Result<TaskId, ControllerError> {
        self.request.try_submit()
    }
}

impl<'a> IntoFuture for Submit<'a, Unwatched, Waiting> {
    type Output = Result<TaskId, ControllerError>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send + 'a>>;

    #[inline]
    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.execute())
    }
}

impl Submit<'_, Unwatched, OwnershipTimed> {
    /// Ownership-bounded admission followed by normal command-queue backpressure.
    ///
    /// A successful [`PreparedSubmission`] cannot produce `NotConfigured`.
    ///
    /// # Errors
    ///
    /// - Returns [`ControllerError::NotConfigured`] when a direct submission has no controller;
    /// - [`ControllerError::ThreadStartFailed`] when cleanup workers cannot start;
    /// - [`ControllerError::ResourceLimit`] when cleanup ownership cannot be granted;
    /// - [`ControllerError::OwnershipAdmissionTimeout`] when cleanup ownership remains unavailable at the configured deadline;
    /// - [`ControllerError::Closed`] when controller intake is closed.
    #[inline]
    pub async fn execute(self) -> Result<TaskId, ControllerError> {
        let wait_for = self._admission.0;
        let (controller, id, spec) = self.request.into_parts()?;
        controller
            .submit_prepared_with_ownership_timeout(id, spec, wait_for)
            .await
    }
}

impl Submit<'_, Watched, Waiting> {
    /// Waiting controller command intake with a final-outcome waiter.
    ///
    /// Success confirms only controller command intake.
    /// [`TaskWaiter::id`] is the submission identity.
    /// [`TaskWaiter::wait`] reports rejection or the admitted task's final outcome.
    /// A successful [`PreparedSubmission`] cannot produce `NotConfigured`.
    ///
    /// # Errors
    ///
    /// - Returns [`ControllerError::NotConfigured`] when a direct submission has no controller;
    /// - [`ControllerError::ThreadStartFailed`] when cleanup workers cannot start;
    /// - [`ControllerError::ResourceLimit`] when cleanup ownership is unavailable;
    /// - [`ControllerError::Closed`] when controller intake is closed.
    #[inline]
    pub async fn execute(self) -> Result<TaskWaiter, ControllerError> {
        let (controller, id, spec) = self.request.into_parts()?;
        let (id, receiver) = controller.submit_prepared_and_watch(id, spec).await?;
        Ok(TaskWaiter::new(id, receiver))
    }

    /// Fail-fast controller command intake with a final-outcome waiter.
    ///
    /// A successful [`PreparedSubmission`] cannot produce `NotConfigured`.
    ///
    /// # Errors
    ///
    /// - Returns [`ControllerError::NotConfigured`] when a direct submission has no controller;
    /// - [`ControllerError::ThreadStartFailed`] when cleanup workers cannot start;
    /// - [`ControllerError::ResourceLimit`] when cleanup ownership is unavailable;
    /// - [`ControllerError::Full`] when controller command capacity is unavailable;
    /// - [`ControllerError::Closed`] when controller intake is closed.
    #[inline(always)]
    pub fn try_intake(self) -> Result<TaskWaiter, ControllerError> {
        let (id, receiver) = self.request.try_submit_and_watch()?;
        Ok(TaskWaiter::new(id, receiver))
    }
}

impl Submit<'_, Watched, OwnershipTimed> {
    /// Ownership-bounded admission with a final-outcome waiter after command intake.
    ///
    /// A successful [`PreparedSubmission`] cannot produce `NotConfigured`.
    ///
    /// # Errors
    ///
    /// - Returns [`ControllerError::NotConfigured`] when a direct submission has no controller;
    /// - [`ControllerError::ThreadStartFailed`] when cleanup workers cannot start;
    /// - [`ControllerError::ResourceLimit`] when cleanup ownership cannot be granted;
    /// - [`ControllerError::OwnershipAdmissionTimeout`] when cleanup ownership remains unavailable at the configured deadline;
    /// - [`ControllerError::Closed`] when controller intake is closed.
    #[inline]
    pub async fn execute(self) -> Result<TaskWaiter, ControllerError> {
        let wait_for = self._admission.0;
        let (controller, id, spec) = self.request.into_parts()?;
        let (id, receiver) = controller
            .submit_prepared_and_watch_with_ownership_timeout(id, spec, wait_for)
            .await?;
        Ok(TaskWaiter::new(id, receiver))
    }
}

impl<Watch, Admission> std::fmt::Debug for Submit<'_, Watch, Admission> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("Submit");

        match &self.request {
            SubmitRequest::Direct { controller, spec } => debug
                .field("controller_configured", &controller.is_some())
                .field("spec", spec),
            SubmitRequest::Prepared { id, spec, .. } => debug.field("id", id).field("spec", spec),
        };

        debug.finish_non_exhaustive()
    }
}

/// A controller request with an identity allocated before intake.
///
/// Obtain this value from [`SupervisorHandle::prepare_submission`](crate::SupervisorHandle::prepare_submission).
/// Record [`id`](Self::id) when the identity is needed before intake. Consume the value with [`submit`](Self::submit).
/// Preparation reserves no task name, slot, queue capacity, or runtime capacity. It publishes no event.
///
/// Dropping this value or the resulting [`Submit`] operation starts no work.
/// Retrying after any terminal intake error requires a new prepared value and a new task identity.
#[must_use = "call submit, then await, execute, or try the resulting operation"]
pub struct PreparedSubmission {
    /// Controller command sender used when the submission operation commits.
    controller: ControllerHandle,

    /// Task ID allocated before any controller command is sent.
    id: TaskId,

    /// Submission specification held until intake.
    spec: ControllerSpec,
}

impl PreparedSubmission {
    pub(crate) fn new(controller: ControllerHandle, spec: ControllerSpec) -> Self {
        Self {
            controller,
            id: TaskId::next(),
            spec,
        }
    }

    /// Preallocated submission identity.
    ///
    /// No event for this identity is published before the operation returned by [`submit`](Self::submit) reaches successful command intake.
    /// The identity does not prove that intake or slot admission occurred.
    #[must_use]
    pub fn id(&self) -> TaskId {
        self.id
    }

    /// Borrowed submission specification before intake.
    #[must_use = "use the prepared controller specification"]
    pub fn spec(&self) -> &ControllerSpec {
        &self.spec
    }

    /// Single-use submission operation preserving the prepared identity.
    ///
    /// Await the default operation directly.
    /// Configure it before finishing with `execute` or `try_intake` when needed.
    #[must_use = "await, execute, or try the prepared submission operation"]
    #[inline]
    pub fn submit(self) -> Submit<'static> {
        let Self {
            controller,
            id,
            spec,
        } = self;
        Submit::prepared(controller, id, spec)
    }
}

impl std::fmt::Debug for PreparedSubmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedSubmission")
            .field("id", &self.id)
            .field("spec", &self.spec)
            .finish_non_exhaustive()
    }
}
