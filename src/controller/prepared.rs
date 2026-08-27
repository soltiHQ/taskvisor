//! Builds explicit controller submission operations.
//!
//! [`SupervisorHandle::submit`](crate::SupervisorHandle::submit) creates a [`Submit`] operation whose terminal method performs controller intake.
//! [`PreparedSubmission`] first exposes the submission identity, then creates the same operation without changing that identity.
//!
//! ```text
//! ControllerSpec ──► Submit ──► execute / try_intake ──► controller intake
//!                         └──► watch ──► TaskWaiter
//! ```
//!
//! Building, configuring, or dropping a submission operation sends no command and starts no work.

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
/// Configure whether the caller needs a final-outcome waiter and whether ownership admission has
/// a deadline, then finish with `execute` or `try_intake`.
/// Every terminal method consumes the operation, including when intake returns an error.
///
/// `Watch` and `Admission` are type states maintained by [`watch`](Self::watch) and
/// [`ownership_timeout`](Self::ownership_timeout). Applications do not need to name them.
#[must_use = "a submission starts no work until execute or try_intake consumes it"]
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
    /// Creates a direct submission without allocating an identity or sending a command.
    ///
    /// A missing controller is reported by the terminal method as
    /// [`ControllerError::NotConfigured`]. Keeping that state in the operation lets
    /// [`SupervisorHandle::submit`](crate::SupervisorHandle::submit) remain infallible.
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
    /// Creates an operation for an identity allocated by [`PreparedSubmission`].
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
    /// Resolves configuration and allocates a direct identity at terminal execution.
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
    /// Requests a direct final-outcome waiter.
    ///
    /// Successful intake returns only [`TaskWaiter`]. Its [`TaskWaiter::id`] is the submission identity.
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
    /// Bounds only cleanup-ownership admission.
    ///
    /// After ownership succeeds, `execute` waits normally for controller command capacity without this deadline.
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
    /// Waits for ownership and command capacity, then confirms controller command intake.
    ///
    /// `Ok(id)` confirms only intake. Slot admission and runtime registration happen later.
    #[inline]
    pub async fn execute(self) -> Result<TaskId, ControllerError> {
        let (controller, id, spec) = self.request.into_parts()?;
        controller.submit_prepared(id, spec).await
    }

    /// Submits only when ownership and controller command capacity are available now.
    ///
    /// This synchronous terminal preserves fail-fast intake.
    /// `Ok(id)` has the same intake-only meaning as [`execute`](Self::execute).
    #[inline(always)]
    pub fn try_intake(self) -> Result<TaskId, ControllerError> {
        self.request.try_submit()
    }
}

impl Submit<'_, Unwatched, OwnershipTimed> {
    /// Bounds ownership admission, then waits normally for controller command capacity.
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
    /// Waits for ownership and command capacity, then returns the final-outcome waiter.
    ///
    /// Success confirms only controller command intake.
    /// Use [`TaskWaiter::id`] for the submission identity and [`TaskWaiter::wait`] for rejection or the admitted task's final outcome.
    #[inline]
    pub async fn execute(self) -> Result<TaskWaiter, ControllerError> {
        let (controller, id, spec) = self.request.into_parts()?;
        let (id, receiver) = controller.submit_prepared_and_watch(id, spec).await?;
        Ok(TaskWaiter::new(id, receiver))
    }

    /// Returns a final-outcome waiter only when intake resources are available now.
    #[inline(always)]
    pub fn try_intake(self) -> Result<TaskWaiter, ControllerError> {
        let (id, receiver) = self.request.try_submit_and_watch()?;
        Ok(TaskWaiter::new(id, receiver))
    }
}

impl Submit<'_, Watched, OwnershipTimed> {
    /// Bounds ownership admission, then returns the final-outcome waiter after command intake.
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
/// Create this value with [`SupervisorHandle::prepare_submission`](crate::SupervisorHandle::prepare_submission),
/// record [`id`](Self::id) if needed, then consume it with [`submit`](Self::submit).
/// Preparation reserves no task name, slot, queue capacity, or runtime capacity and publishes no event.
///
/// Dropping this value or the resulting [`Submit`] operation starts no work.
/// Retrying after any terminal intake error requires a new prepared value and a new task identity.
#[must_use = "a prepared submission starts no work until submit creates an executed operation"]
pub struct PreparedSubmission {
    /// Controller command sender used when the submission operation executes.
    controller: ControllerHandle,

    /// Task ID allocated before any controller command is sent.
    id: TaskId,

    /// Submission specification held until intake.
    spec: ControllerSpec,
}

impl PreparedSubmission {
    /// Allocates a task identity without sending a controller command.
    pub(crate) fn new(controller: ControllerHandle, spec: ControllerSpec) -> Self {
        Self {
            controller,
            id: TaskId::next(),
            spec,
        }
    }

    /// Returns the identity allocated for this submission.
    ///
    /// No event for this identity is published before the operation returned by [`submit`](Self::submit)
    /// reaches a successful command intake. The identity does not prove that intake or slot admission occurred.
    #[must_use]
    pub fn id(&self) -> TaskId {
        self.id
    }

    /// Returns the submission specification without sending it.
    #[must_use = "use the prepared controller specification"]
    pub fn spec(&self) -> &ControllerSpec {
        &self.spec
    }

    /// Creates the explicit submission operation while preserving this prepared identity.
    ///
    /// Configure the returned [`Submit`] and finish with `execute` or `try_intake`.
    #[must_use = "execute or try the prepared submission operation"]
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
