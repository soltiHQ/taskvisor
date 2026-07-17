//! A controller submission whose identity is visible before intake.

use super::{ControllerError, ControllerSpec, core::ControllerHandle};
use crate::{TaskId, TaskWaiter};

/// A single-use controller submission with a preallocated [`TaskId`].
///
/// Create this value with [`SupervisorHandle::prepare_submission`](crate::SupervisorHandle::prepare_submission).
/// Preparation allocates the identity but does not enqueue work or publish an event.
/// This lets an application install its own correlation mapping before the controller can emit an event for the submission.
///
/// Call [`submit`](Self::submit), [`try_submit`](Self::try_submit), [`submit_and_watch`](Self::submit_and_watch), or
/// [`try_submit_and_watch`](Self::try_submit_and_watch) to consume the value and commit it to the same controller path used by [`SupervisorHandle`](crate::SupervisorHandle).
/// Dropping it without submitting starts no work and publishes no event.
///
/// This type is intentionally not [`Clone`]. One prepared value can commit at most one controller submission.
///
/// ## Example
///
/// ```rust,no_run
/// use taskvisor::prelude::*;
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let supervisor = Supervisor::builder(SupervisorConfig::default())
///     .with_controller(ControllerConfig::default())
///     .build();
/// let handle = supervisor.serve();
///
/// let task = TaskFn::arc("sync-tenant-42", |_ctx| async { Ok(()) });
/// let request = ControllerSpec::replace(TaskSpec::once(task)).with_slot("tenant-42");
/// let prepared = handle.prepare_submission(request)?;
/// let id = prepared.id();
///
/// // Store application_id -> id here. No event for `id` can exist yet.
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
    controller: ControllerHandle,
    id: TaskId,
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

    /// Returns the identity reserved for this submission.
    ///
    /// Preparation itself emits no event.
    ///
    /// After a submit method is called, this identity is used unchanged through controller admission, registry execution, events, cancellation, and the final outcome.
    #[must_use]
    pub fn id(&self) -> TaskId {
        self.id
    }

    /// Returns the controller specification that will be submitted.
    #[must_use = "use the prepared controller specification"]
    pub fn spec(&self) -> &ControllerSpec {
        &self.spec
    }

    /// Waits for controller-command capacity and commits this submission.
    ///
    /// `Ok(id)` confirms only controller intake. Slot admission and registry registration happen later.
    ///
    /// # Errors
    ///
    /// Returns [`ControllerError::Closed`] if the controller command channel is closed.
    pub async fn submit(self) -> Result<TaskId, ControllerError> {
        let Self {
            controller,
            id,
            spec,
        } = self;
        controller.submit_prepared(id, spec).await
    }

    /// Commits this submission only if controller-command capacity is available now.
    ///
    /// # Errors
    ///
    /// - [`ControllerError::Full`] when the controller command queue has no capacity.
    /// - [`ControllerError::Closed`] when the controller command channel is closed.
    pub fn try_submit(self) -> Result<TaskId, ControllerError> {
        let Self {
            controller,
            id,
            spec,
        } = self;
        controller.try_submit_prepared(id, spec)
    }

    /// Waits for controller-command capacity, commits this submission, and returns its final-outcome waiter.
    ///
    /// The waiter has the same reliable completion semantics as [`SupervisorHandle::submit_and_watch`](crate::SupervisorHandle::submit_and_watch).
    ///
    /// # Errors
    ///
    /// Returns [`ControllerError::Closed`] if the controller command channel is closed.
    pub async fn submit_and_watch(self) -> Result<(TaskId, TaskWaiter), ControllerError> {
        let Self {
            controller,
            id,
            spec,
        } = self;
        let (submitted_id, rx) = controller.submit_prepared_and_watch(id, spec).await?;
        Ok((submitted_id, TaskWaiter::new(submitted_id, rx)))
    }

    /// Commits this watched submission only if controller-command capacity is available now.
    ///
    /// # Errors
    ///
    /// - [`ControllerError::Full`] when the controller command queue has no capacity.
    /// - [`ControllerError::Closed`] when the controller command channel is closed.
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
