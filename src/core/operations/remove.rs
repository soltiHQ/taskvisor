//! Defines task removal without a terminal-cleanup wait.

use crate::RuntimeError;

use super::{FailFast, TaskTarget, Waiting};
use crate::core::SupervisorHandle;

/// A task-removal operation with typed command-queue policy.
///
/// Success waits for the removal claim decision but not for terminal task cleanup.
/// Use a cancellation operation when terminal confirmation is required.
#[must_use = "a remove operation starts no work until `.execute()` is awaited"]
pub struct RemoveOperation<'a, Admission = Waiting, Target = TaskTarget> {
    handle: &'a SupervisorHandle,
    target: Target,
    _admission: Admission,
}

impl<Admission, Target> std::fmt::Debug for RemoveOperation<'_, Admission, Target>
where
    Admission: std::fmt::Debug,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoveOperation")
            .field("target_type", &std::any::type_name::<Target>())
            .field("admission", &self._admission)
            .finish_non_exhaustive()
    }
}

impl<'a, Target> RemoveOperation<'a, Waiting, Target> {
    #[inline]
    pub(crate) fn new(handle: &'a SupervisorHandle, target: Target) -> Self {
        Self {
            handle,
            target,
            _admission: Waiting,
        }
    }

    /// Immediate admission to the required management queue.
    #[inline]
    pub fn fail_fast(self) -> RemoveOperation<'a, FailFast, Target> {
        RemoveOperation {
            handle: self.handle,
            target: self.target,
            _admission: FailFast,
        }
    }
}

impl<Target> RemoveOperation<'_, Waiting, Target>
where
    Target: Into<TaskTarget>,
{
    /// Removal-claim result after waiting command-queue admission.
    ///
    /// # Errors
    ///
    /// - Returns [`RuntimeError::ShuttingDown`] when runtime intake is closed;
    /// - [`RuntimeError::ResourceLimitReached`] when a [`TaskTarget::Id`] routed through a configured controller exhausts the identity-operation budget.
    #[inline]
    pub async fn execute(self) -> Result<bool, RuntimeError> {
        match self.target.into() {
            TaskTarget::Id(id) => {
                #[cfg(feature = "controller")]
                if let Some(controller) = self.handle.controller() {
                    return controller.handle().remove(id).await;
                }
                self.handle.core().remove(id).await
            }
            TaskTarget::Name(name) => self.handle.core().remove_by_name(name).await,
        }
    }
}

impl<Target> RemoveOperation<'_, FailFast, Target>
where
    Target: Into<TaskTarget>,
{
    /// Removal-claim result after immediate command-queue admission.
    ///
    /// # Errors
    ///
    /// - Returns [`RuntimeError::ShuttingDown`] when runtime intake is closed;
    /// - [`RuntimeError::CommandQueueFull`] when the required management queue has no capacity;
    /// - [`RuntimeError::ResourceLimitReached`] when a [`TaskTarget::Id`] routed through a configured controller exhausts the identity-operation budget.
    #[inline]
    pub async fn execute(self) -> Result<bool, RuntimeError> {
        match self.target.into() {
            TaskTarget::Id(id) => {
                #[cfg(feature = "controller")]
                if let Some(controller) = self.handle.controller() {
                    return controller.handle().try_remove(id).await;
                }
                self.handle.core().try_remove(id).await
            }
            TaskTarget::Name(name) => self.handle.core().try_remove_by_name(name).await,
        }
    }
}
