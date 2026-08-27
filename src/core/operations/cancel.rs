//! Builds one terminal task-cancellation operation.

use std::time::Duration;

use crate::RuntimeError;

use super::{FailFast, TaskTarget, TerminationTimed, TerminationUnbounded, Waiting};
use crate::core::SupervisorHandle;

/// A task-cancellation operation with independent queue and termination-wait policy.
///
/// [`fail_fast`](Self::fail_fast) changes only command-queue admission.
/// [`termination_timeout`](Self::termination_timeout) changes only the later wait for logical terminal cleanup.
/// The two settings can be applied in either order.
#[must_use = "a cancel operation starts no work until `.execute()` is awaited"]
pub struct CancelOperation<
    'a,
    Admission = Waiting,
    Termination = TerminationUnbounded,
    Target = TaskTarget,
> {
    handle: &'a SupervisorHandle,
    target: Target,
    _admission: Admission,
    termination: Termination,
}

impl<Admission, Termination, Target> std::fmt::Debug
    for CancelOperation<'_, Admission, Termination, Target>
where
    Admission: std::fmt::Debug,
    Termination: std::fmt::Debug,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CancelOperation")
            .field("target_type", &std::any::type_name::<Target>())
            .field("admission", &self._admission)
            .field("termination", &self.termination)
            .finish_non_exhaustive()
    }
}

impl<'a, Target> CancelOperation<'a, Waiting, TerminationUnbounded, Target> {
    /// Creates an operation that waits for queue capacity and terminal cleanup.
    #[inline]
    pub(crate) fn new(handle: &'a SupervisorHandle, target: Target) -> Self {
        Self {
            handle,
            target,
            _admission: Waiting,
            termination: TerminationUnbounded,
        }
    }
}

impl<'a, Termination, Target> CancelOperation<'a, Waiting, Termination, Target> {
    /// Uses immediate admission to the required management queue.
    #[inline]
    pub fn fail_fast(self) -> CancelOperation<'a, FailFast, Termination, Target> {
        CancelOperation {
            handle: self.handle,
            target: self.target,
            _admission: FailFast,
            termination: self.termination,
        }
    }
}

impl<'a, Admission, Target> CancelOperation<'a, Admission, TerminationUnbounded, Target> {
    /// Limits only this caller's wait for logical terminal cleanup.
    ///
    /// Queue admission and the cancellation claim are outside this deadline.
    /// A timeout does not undo an already committed cancellation.
    #[inline]
    pub fn termination_timeout(
        self,
        wait_for: Duration,
    ) -> CancelOperation<'a, Admission, TerminationTimed, Target> {
        CancelOperation {
            handle: self.handle,
            target: self.target,
            _admission: self._admission,
            termination: TerminationTimed(wait_for),
        }
    }
}

impl<Target> CancelOperation<'_, Waiting, TerminationUnbounded, Target>
where
    Target: Into<TaskTarget>,
{
    /// Waits for queue capacity, the cancellation decision, and logical terminal cleanup.
    #[inline]
    pub async fn execute(self) -> Result<bool, RuntimeError> {
        match self.target.into() {
            TaskTarget::Id(id) => {
                #[cfg(feature = "controller")]
                if let Some(controller) = self.handle.controller() {
                    return controller.handle().cancel(id).await;
                }
                self.handle.core().cancel(id).await
            }
            TaskTarget::Name(name) => self.handle.core().cancel_by_name(name).await,
        }
    }
}

impl<Target> CancelOperation<'_, FailFast, TerminationUnbounded, Target>
where
    Target: Into<TaskTarget>,
{
    /// Uses immediate queue admission, then waits without a termination deadline.
    #[inline]
    pub async fn execute(self) -> Result<bool, RuntimeError> {
        match self.target.into() {
            TaskTarget::Id(id) => {
                #[cfg(feature = "controller")]
                if let Some(controller) = self.handle.controller() {
                    return controller.handle().try_cancel(id).await;
                }
                self.handle.core().try_cancel(id).await
            }
            TaskTarget::Name(name) => self.handle.core().try_cancel_by_name(name).await,
        }
    }
}

impl<Target> CancelOperation<'_, Waiting, TerminationTimed, Target>
where
    Target: Into<TaskTarget>,
{
    /// Waits for queue admission and bounds only the later terminal-cleanup wait.
    #[inline]
    pub async fn execute(self) -> Result<bool, RuntimeError> {
        let wait_for = self.termination.0;
        match self.target.into() {
            TaskTarget::Id(id) => {
                #[cfg(feature = "controller")]
                if let Some(controller) = self.handle.controller() {
                    return controller.handle().cancel_with_timeout(id, wait_for).await;
                }
                self.handle.core().cancel_with_timeout(id, wait_for).await
            }
            TaskTarget::Name(name) => {
                self.handle
                    .core()
                    .cancel_by_name_with_timeout(name, wait_for)
                    .await
            }
        }
    }
}

impl<Target> CancelOperation<'_, FailFast, TerminationTimed, Target>
where
    Target: Into<TaskTarget>,
{
    /// Combines immediate queue admission with a bounded terminal-cleanup wait.
    #[inline]
    pub async fn execute(self) -> Result<bool, RuntimeError> {
        let wait_for = self.termination.0;
        match self.target.into() {
            TaskTarget::Id(id) => {
                #[cfg(feature = "controller")]
                if let Some(controller) = self.handle.controller() {
                    return controller
                        .handle()
                        .try_cancel_with_timeout(id, wait_for)
                        .await;
                }
                self.handle
                    .core()
                    .try_cancel_with_timeout(id, wait_for)
                    .await
            }
            TaskTarget::Name(name) => {
                self.handle
                    .core()
                    .try_cancel_by_name_with_timeout(name, wait_for)
                    .await
            }
        }
    }
}
