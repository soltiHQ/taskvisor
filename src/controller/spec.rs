//! # Submission specification for the [`Controller`](super::Controller).
//!
//! [`ControllerSpec`] pairs an [`AdmissionPolicy`] with a [`TaskSpec`] to describe
//! what to run and how to handle concurrent submissions to the same slot.

use std::sync::Arc;

use super::admission::AdmissionPolicy;
use crate::TaskSpec;

/// Request to submit a task to the controller.
///
/// Combines an admission policy and the actual task specification.
/// The slot name is admission metadata on the submission (set via [`with_slot`](Self::with_slot)),
/// falling back to the task name; read it via [`slot_name`](Self::slot_name).
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

    /// Admission slot key; `None` falls back to the task name. Set via [`with_slot`](Self::with_slot).
    slot: Option<Arc<str>>,
}

impl std::fmt::Debug for ControllerSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControllerSpec")
            .field("admission", &self.admission)
            .field("task_spec", &self.task_spec)
            .field("slot", &self.slot_name())
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
            slot: None,
        }
    }

    /// Builder: set the admission slot key.
    ///
    /// Defaults to the task name. The slot is the controller's concurrency unit
    /// (one running task per slot); it is admission metadata, not part of the
    /// task's execution model.
    pub fn with_slot(mut self, slot: impl Into<Arc<str>>) -> Self {
        self.slot = Some(slot.into());
        self
    }

    /// Returns the slot name (admission key); falls back to the task name when unset.
    pub fn slot_name(&self) -> &str {
        self.slot
            .as_deref()
            .unwrap_or_else(|| self.task_spec.name())
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
    use crate::TaskContext;
    use crate::{BackoffPolicy, RestartPolicy, TaskFn, TaskRef};

    fn make_spec(name: &str) -> TaskSpec {
        let task: TaskRef = TaskFn::arc(name, |_ctx: TaskContext| async { Ok(()) });
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
    fn slot_name_falls_back_to_task_name() {
        let cs = ControllerSpec::queue(make_spec("my-slot"));
        assert_eq!(cs.slot_name(), "my-slot");
    }

    #[test]
    fn slot_name_uses_explicit_slot() {
        let cs = ControllerSpec::queue(make_spec("runner-web-7")).with_slot("web");
        assert_eq!(cs.slot_name(), "web");
        assert_eq!(cs.task_spec.name(), "runner-web-7");
    }
}
