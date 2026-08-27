//! Defines direct task registration before registry admission.

use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::time::Duration;

use crate::{RuntimeError, TaskId, TaskSpec, TaskWaiter};

use super::{FailFast, OwnershipTimed, Unwatched, Waiting, Watched};
use crate::core::SupervisorCore;

/// A direct task-registration operation with typed outcome and admission policy.
///
/// The default waiting, unwatched operation can be awaited directly.
/// Configured operations require `execute`.
/// APIs that accept a [`Future`] require `execute()` or [`IntoFuture::into_future`].
/// Dropping the operation starts no work.
/// [`watch`](Self::watch) changes the terminal result from [`TaskId`] to [`TaskWaiter`].
/// [`ownership_timeout`](Self::ownership_timeout) and [`fail_fast`](Self::fail_fast) are mutually exclusive and therefore cannot both be selected.
#[must_use = "await the default add operation or call and await `.execute()`"]
pub struct AddOperation<'a, Watch = Unwatched, Admission = Waiting> {
    core: &'a SupervisorCore,
    spec: TaskSpec,
    watch: Watch,
    _admission: Admission,
}

impl<Watch, Admission> std::fmt::Debug for AddOperation<'_, Watch, Admission>
where
    Watch: std::fmt::Debug,
    Admission: std::fmt::Debug,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AddOperation")
            .field("spec", &self.spec)
            .field("watch", &self.watch)
            .field("admission", &self._admission)
            .finish_non_exhaustive()
    }
}

impl<'a> AddOperation<'a, Unwatched, Waiting> {
    /// Creates the default waiting, unwatched operation for one handle call.
    #[inline]
    pub(crate) fn new(core: &'a SupervisorCore, spec: TaskSpec) -> Self {
        Self {
            core,
            spec,
            watch: Unwatched,
            _admission: Waiting,
        }
    }
}

impl<'a, Admission> AddOperation<'a, Unwatched, Admission> {
    /// Final-outcome delivery through [`TaskWaiter`] instead of [`TaskId`].
    #[inline]
    pub fn watch(self) -> AddOperation<'a, Watched, Admission> {
        AddOperation {
            core: self.core,
            spec: self.spec,
            watch: Watched,
            _admission: self._admission,
        }
    }
}

impl<'a, Watch> AddOperation<'a, Watch, Waiting> {
    /// Deadline for cleanup-ownership admission before registry command commit.
    ///
    /// Once ownership is available, command-queue admission and the registry decision have no deadline from this setting.
    #[inline]
    pub fn ownership_timeout(self, wait_for: Duration) -> AddOperation<'a, Watch, OwnershipTimed> {
        AddOperation {
            core: self.core,
            spec: self.spec,
            watch: self.watch,
            _admission: OwnershipTimed(wait_for),
        }
    }

    /// Immediate cleanup-ownership and registry-command admission.
    ///
    /// Execution still waits for the authoritative registry decision after command commit.
    #[inline]
    pub fn fail_fast(self) -> AddOperation<'a, Watch, FailFast> {
        AddOperation {
            core: self.core,
            spec: self.spec,
            watch: self.watch,
            _admission: FailFast,
        }
    }
}

impl AddOperation<'_, Unwatched, Waiting> {
    /// Task identity after waiting admission and successful registry registration.
    ///
    /// Awaiting the default operation directly is equivalent to calling this method.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::ThreadStartFailed`] when cleanup workers cannot start;
    /// - [`RuntimeError::ResourceLimitReached`] when ownership capacity is exhausted;
    /// - [`RuntimeError::ResourceLimitReached`] when registry capacity is exhausted;
    /// - [`RuntimeError::ShuttingDown`] when runtime intake is closed;
    /// - [`RuntimeError::TaskAlreadyExists`] when the task name is already reserved.
    #[inline]
    pub async fn execute(self) -> Result<TaskId, RuntimeError> {
        self.core.add_task(self.spec).await
    }
}

impl<'a> IntoFuture for AddOperation<'a, Unwatched, Waiting> {
    type Output = Result<TaskId, RuntimeError>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send + 'a>>;

    #[inline]
    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.execute())
    }
}

impl AddOperation<'_, Unwatched, OwnershipTimed> {
    /// Task identity after bounded ownership admission and successful registry registration.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::ThreadStartFailed`] when cleanup workers cannot start;
    /// - [`RuntimeError::ResourceLimitReached`] when ownership capacity is exhausted;
    /// - [`RuntimeError::ResourceLimitReached`] when registry capacity is exhausted;
    /// - [`RuntimeError::OwnershipAdmissionTimeout`] when ownership remains unavailable for the configured duration;
    /// - [`RuntimeError::ShuttingDown`] when runtime intake is closed;
    /// - [`RuntimeError::TaskAlreadyExists`] when the task name is already reserved.
    #[inline]
    pub async fn execute(self) -> Result<TaskId, RuntimeError> {
        self.core
            .add_task_with_ownership_timeout(self.spec, self._admission.0)
            .await
    }
}

impl AddOperation<'_, Unwatched, FailFast> {
    /// Task identity after immediate bounded admission and successful registry registration.
    ///
    /// # Errors
    ///
    /// - [`RuntimeError::ThreadStartFailed`] when cleanup workers cannot start;
    /// - [`RuntimeError::ResourceLimitReached`] when ownership capacity is unavailable;
    /// - [`RuntimeError::ResourceLimitReached`] when registry capacity is exhausted;
    /// - [`RuntimeError::CommandQueueFull`] when registry command capacity is not immediately available;
    /// - [`RuntimeError::ShuttingDown`] when runtime intake is closed;
    /// - [`RuntimeError::TaskAlreadyExists`] when the task name is already reserved.
    #[inline]
    pub async fn execute(self) -> Result<TaskId, RuntimeError> {
        self.core.try_add_task(self.spec).await
    }
}

impl AddOperation<'_, Watched, Waiting> {
    /// Final-outcome waiter after waiting admission and successful registry registration.
    ///
    /// # Errors
    ///
    /// - Returns [`RuntimeError::ThreadStartFailed`] when cleanup workers cannot start;
    /// - [`RuntimeError::ResourceLimitReached`] when ownership capacity is exhausted;
    /// - [`RuntimeError::ResourceLimitReached`] when registry capacity is exhausted;
    /// - [`RuntimeError::ShuttingDown`] when runtime intake is closed;
    /// - [`RuntimeError::TaskAlreadyExists`] when the task name is already reserved.
    #[inline]
    pub async fn execute(self) -> Result<TaskWaiter, RuntimeError> {
        let (id, receiver) = self.core.add_task_watched(self.spec).await?;
        Ok(TaskWaiter::new(id, receiver))
    }
}

impl AddOperation<'_, Watched, OwnershipTimed> {
    /// Final-outcome waiter after bounded ownership admission and successful registry registration.
    ///
    /// # Errors
    ///
    /// - Returns [`RuntimeError::ThreadStartFailed`] when cleanup workers cannot start;
    /// - [`RuntimeError::ResourceLimitReached`] when ownership capacity is exhausted;
    /// - [`RuntimeError::ResourceLimitReached`] when registry capacity is exhausted;
    /// - [`RuntimeError::OwnershipAdmissionTimeout`] when ownership remains unavailable for the configured duration;
    /// - [`RuntimeError::ShuttingDown`] when runtime intake is closed;
    /// - [`RuntimeError::TaskAlreadyExists`] when the task name is already reserved.
    #[inline]
    pub async fn execute(self) -> Result<TaskWaiter, RuntimeError> {
        let (id, receiver) = self
            .core
            .add_task_watched_with_ownership_timeout(self.spec, self._admission.0)
            .await?;
        Ok(TaskWaiter::new(id, receiver))
    }
}

impl AddOperation<'_, Watched, FailFast> {
    /// Final-outcome waiter after immediate bounded admission and successful registry registration.
    ///
    /// # Errors
    ///
    /// - Returns [`RuntimeError::ThreadStartFailed`] when cleanup workers cannot start;
    /// - [`RuntimeError::ResourceLimitReached`] when ownership capacity is unavailable;
    /// - [`RuntimeError::ResourceLimitReached`] when registry capacity is exhausted;
    /// - [`RuntimeError::CommandQueueFull`] when registry command capacity is not immediately available;
    /// - [`RuntimeError::ShuttingDown`] when runtime intake is closed;
    /// - [`RuntimeError::TaskAlreadyExists`] when the task name is already reserved.
    #[inline]
    pub async fn execute(self) -> Result<TaskWaiter, RuntimeError> {
        let (id, receiver) = self.core.try_add_task_watched(self.spec).await?;
        Ok(TaskWaiter::new(id, receiver))
    }
}
