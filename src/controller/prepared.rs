//! Exposes a submission ID before controller intake and events can begin.
//!
//! [`SupervisorHandle::prepare_submission`](crate::SupervisorHandle::prepare_submission)
//! allocates a [`TaskId`] and returns it with the unsubmitted [`ControllerSpec`].
//! This lets an application map its own ID to the `TaskId` before any event for
//! that task can be published.
//!
//! ```text
//! prepare_submission
//!      ├── application ──► read and store TaskId
//!      └── PreparedSubmission ──► submit* ──► controller intake
//! ```
//!
//! Preparation does not reserve the task name, slot, queue capacity, or runtime
//! capacity. It sends no command and starts no work. A submit method consumes
//! the prepared value and performs normal controller intake. Use direct
//! controller `submit*` methods when the ID is not needed beforehand.

use super::{ControllerError, ControllerSpec, engine::ControllerHandle};
use crate::{TaskId, TaskWaiter};

/// A single-use controller request with an ID allocated before intake.
///
/// Create this value with
/// [`SupervisorHandle::prepare_submission`](crate::SupervisorHandle::prepare_submission).
/// Use [`id`](Self::id) to record correlation before controller events can start.
///
/// Each submit method consumes this value, including when intake returns an
/// error. Retrying requires a new prepared value and a new task ID. Dropping a
/// prepared value without submitting starts no work and publishes no event.
///
/// # Examples
///
/// ```rust,no_run
/// use taskvisor::prelude::*;
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let supervisor = Supervisor::builder(SupervisorConfig::default())
///     .with_controller(ControllerConfig::default())
///     .build();
/// let handle = supervisor.serve()?;
///
/// let task = TaskFn::arc(|_ctx| async { Ok(()) });
/// let request = ControllerSpec::replace(TaskSpec::once("sync-tenant-42", task))
///     .with_slot("tenant-42");
/// let prepared = handle.prepare_submission(request)?;
/// let id = prepared.id();
///
/// let (submitted_id, waiter) = prepared.submit_and_watch().await?;
/// assert_eq!(submitted_id, id);
/// assert!(waiter.wait().await?.is_success());
///
/// handle.shutdown().await?;
/// # Ok(())
/// # }
/// ```
#[must_use = "a prepared submission starts no work until a submit method consumes it"]
pub struct PreparedSubmission {
    /// Controller command sender used when this value is consumed.
    controller: ControllerHandle,

    /// Task ID allocated before any controller command is sent.
    id: TaskId,

    /// Submission specification held until intake.
    spec: ControllerSpec,
}

impl PreparedSubmission {
    /// Allocates a task ID without sending a controller command.
    pub(crate) fn new(controller: ControllerHandle, spec: ControllerSpec) -> Self {
        Self {
            controller,
            id: TaskId::next(),
            spec,
        }
    }

    /// Returns the task ID allocated for this submission.
    ///
    /// No event for this ID is published before a submit method consumes this
    /// value. After intake, the same ID identifies controller admission, events,
    /// cancellation, and the final outcome. An admitted runtime task uses it too.
    ///
    /// This ID does not prove that command intake or slot admission occurred.
    #[must_use]
    pub fn id(&self) -> TaskId {
        self.id
    }

    /// Returns the submission specification without sending it.
    #[must_use = "use the prepared controller specification"]
    pub fn spec(&self) -> &ControllerSpec {
        &self.spec
    }

    /// Waits for intake resources and submits without a final-outcome waiter.
    ///
    /// The returned ID is the value from [`id`](Self::id). Success confirms
    /// command intake. Slot admission and runtime registration happen later.
    /// Use [`submit_and_watch`](Self::submit_and_watch) when the caller needs
    /// the later admission rejection or final task outcome.
    ///
    /// # Errors
    ///
    /// Returns [`ControllerError::ThreadStartFailed`] or
    /// [`ControllerError::ResourceLimit`] when Taskvisor cannot reserve cleanup
    /// ownership for the task. Returns [`ControllerError::Closed`] when the
    /// controller command channel closes before intake.
    pub async fn submit(self) -> Result<TaskId, ControllerError> {
        let Self {
            controller,
            id,
            spec,
        } = self;
        controller.submit_prepared(id, spec).await
    }

    /// Submits without waiting for intake capacity or returning a waiter.
    ///
    /// Success has the same intake-only meaning as [`submit`](Self::submit).
    /// Use this method when the caller has its own backpressure or retry policy.
    ///
    /// # Errors
    ///
    /// Returns the intake errors from [`submit`](Self::submit). It also returns
    /// [`ControllerError::Full`] when the command queue has no capacity.
    pub fn try_submit(self) -> Result<TaskId, ControllerError> {
        let Self {
            controller,
            id,
            spec,
        } = self;
        controller.try_submit_prepared(id, spec)
    }

    /// Waits for intake resources and returns a final-outcome waiter.
    ///
    /// Success confirms command intake. The waiter later reports
    /// [`TaskOutcome::Rejected`](crate::TaskOutcome::Rejected) for admission
    /// rejection, or the final outcome of an admitted task. Call
    /// [`TaskWaiter::wait`] to receive it.
    ///
    /// # Errors
    ///
    /// Returns the same intake errors as [`submit`](Self::submit).
    pub async fn submit_and_watch(self) -> Result<(TaskId, TaskWaiter), ControllerError> {
        let Self {
            controller,
            id,
            spec,
        } = self;
        let (submitted_id, rx) = controller.submit_prepared_and_watch(id, spec).await?;
        Ok((submitted_id, TaskWaiter::new(submitted_id, rx)))
    }

    /// Returns a waiter without waiting for intake capacity.
    ///
    /// Success confirms command intake. The returned waiter has the same
    /// contract as [`submit_and_watch`](Self::submit_and_watch). Use this method
    /// when the caller needs the final result and owns its backpressure policy.
    ///
    /// # Errors
    ///
    /// Returns the intake errors from
    /// [`submit_and_watch`](Self::submit_and_watch). It also returns
    /// [`ControllerError::Full`] when the command queue has no capacity.
    pub fn try_submit_and_watch(self) -> Result<(TaskId, TaskWaiter), ControllerError> {
        let Self {
            controller,
            id,
            spec,
        } = self;
        let (submitted_id, rx) = controller.try_submit_prepared_and_watch(id, spec)?;
        Ok((submitted_id, TaskWaiter::new(submitted_id, rx)))
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
