//! Builds one direct registry-add operation.

use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::time::Duration;

use crate::{RuntimeError, TaskId, TaskSpec, TaskWaiter};

use super::{FailFast, OwnershipTimed, Unwatched, Waiting, Watched};
use crate::core::SupervisorCore;

/// A direct task-registration operation with typed outcome and admission policy.
///
/// Await the default waiting, unwatched operation directly, or call `execute` explicitly to
/// commit any configured operation.
/// Direct await creates one boxed `Send` future; `execute` avoids that shorthand wrapper.
/// APIs that accept a [`Future`] require `execute()` or [`IntoFuture::into_future`].
/// Dropping the builder starts no work.
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
    /// Returns the final-outcome waiter instead of returning the task identity directly.
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
    /// Bounds only cleanup-ownership admission before registry command commit.
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

    /// Uses immediate ownership and registry-queue admission.
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
    /// Waits for admission and returns the identity after registry registration succeeds.
    ///
    /// Awaiting the default operation directly is equivalent to calling this method.
    #[inline]
    pub async fn execute(self) -> Result<TaskId, RuntimeError> {
        self.core.add_task(self.spec).await
    }
}

impl<'a> IntoFuture for AddOperation<'a, Unwatched, Waiting> {
    type Output = Result<TaskId, RuntimeError>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send + 'a>>;

    /// Executes the default waiting, unwatched add operation.
    #[inline]
    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.execute())
    }
}

impl AddOperation<'_, Unwatched, OwnershipTimed> {
    /// Bounds ownership admission, then returns the identity after registry registration succeeds.
    #[inline]
    pub async fn execute(self) -> Result<TaskId, RuntimeError> {
        self.core
            .add_task_with_ownership_timeout(self.spec, self._admission.0)
            .await
    }
}

impl AddOperation<'_, Unwatched, FailFast> {
    /// Uses fail-fast bounded admission, then returns the registry's decision.
    #[inline]
    pub async fn execute(self) -> Result<TaskId, RuntimeError> {
        self.core.try_add_task(self.spec).await
    }
}

impl AddOperation<'_, Watched, Waiting> {
    /// Waits for admission and returns a waiter after registry registration succeeds.
    #[inline]
    pub async fn execute(self) -> Result<TaskWaiter, RuntimeError> {
        let (id, receiver) = self.core.add_task_watched(self.spec).await?;
        Ok(TaskWaiter::new(id, receiver))
    }
}

impl AddOperation<'_, Watched, OwnershipTimed> {
    /// Bounds ownership admission and returns a waiter after registry registration succeeds.
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
    /// Uses fail-fast bounded admission and returns a waiter after registration succeeds.
    #[inline]
    pub async fn execute(self) -> Result<TaskWaiter, RuntimeError> {
        let (id, receiver) = self.core.try_add_task_watched(self.spec).await?;
        Ok(TaskWaiter::new(id, receiver))
    }
}
