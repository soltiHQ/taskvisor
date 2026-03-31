//! # Submission specification for the [`Controller`](super::Controller).
//!
//! [`ControllerSpec`] pairs an [`AdmissionPolicy`] with a [`TaskSpec`] to describe
//! what to run and how to handle concurrent submissions to the same slot.

use super::admission::AdmissionPolicy;
use crate::TaskSpec;

/// Request to submit a task to the controller.
///
/// Combines an admission policy and the actual task specification.
/// The slot name is derived from the task name via [`slot_name`](Self::slot_name).
///
/// # Also
///
/// - [`AdmissionPolicy`] - how concurrent submissions are handled (Queue/Replace/DropIfRunning)
/// - [`TaskSpec`](crate::TaskSpec) - task configuration (restart, backoff, timeout)
/// - `Controller` - accepts submissions via [`SupervisorHandle::submit`](crate::SupervisorHandle::submit)
#[derive(Clone)]
#[must_use]
pub struct ControllerSpec {
    /// Admission policy.
    pub admission: AdmissionPolicy,

    /// Task specification to run.
    pub task_spec: TaskSpec,
}

impl std::fmt::Debug for ControllerSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControllerSpec")
            .field("admission", &self.admission)
            .field("task_spec", &self.task_spec)
            .finish()
    }
}

impl ControllerSpec {
    /// Creates a new controller submission specification.
    ///
    /// ## Parameters
    /// - `admission`: How to handle concurrent submissions
    /// - `task_spec`: The task to execute
    pub fn new(admission: AdmissionPolicy, task_spec: TaskSpec) -> Self {
        Self {
            admission,
            task_spec,
        }
    }

    /// Returns the slot name (derived from the task name).
    pub fn slot_name(&self) -> &str {
        self.task_spec.name()
    }

    /// Convenience: Queue admission.
    #[inline]
    pub fn queue(task_spec: TaskSpec) -> Self {
        Self::new(AdmissionPolicy::Queue, task_spec)
    }

    /// Convenience: Replace admission (latest-wins).
    #[inline]
    pub fn replace(task_spec: TaskSpec) -> Self {
        Self::new(AdmissionPolicy::Replace, task_spec)
    }

    /// Convenience: DropIfRunning admission (skip if slot busy).
    #[inline]
    pub fn drop_if_running(task_spec: TaskSpec) -> Self {
        Self::new(AdmissionPolicy::DropIfRunning, task_spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BackoffPolicy, RestartPolicy, TaskFn, TaskRef};
    use tokio_util::sync::CancellationToken;

    fn make_spec(name: &str) -> TaskSpec {
        let task: TaskRef = TaskFn::arc(name, |_ctx: CancellationToken| async { Ok(()) });
        TaskSpec::new(task, RestartPolicy::Never, BackoffPolicy::default(), None)
    }

    #[test]
    fn convenience_constructors_set_correct_policy() {
        assert_eq!(
            ControllerSpec::queue(make_spec("t")).admission,
            AdmissionPolicy::Queue
        );
        assert_eq!(
            ControllerSpec::replace(make_spec("t")).admission,
            AdmissionPolicy::Replace
        );
        assert_eq!(
            ControllerSpec::drop_if_running(make_spec("t")).admission,
            AdmissionPolicy::DropIfRunning
        );
    }

    #[test]
    fn slot_name_equals_task_name() {
        let cs = ControllerSpec::queue(make_spec("my-slot"));
        assert_eq!(cs.slot_name(), "my-slot");
    }
}
