use super::admission::AdmissionPolicy;
use crate::TaskSpec;

/// Request to submit a task to the controller.
///
/// Combines a slot name, admission policy, and the actual task specification.
#[derive(Clone)]
pub struct ControllerSpec {
    /// Admission policy.
    pub admission: AdmissionPolicy,

    /// Task specification to run.
    pub task_spec: TaskSpec,
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

    /// Returns the slot name.
    pub fn slot_name(&self) -> &str {
        self.task_spec.name()
    }

    /// Convenience: Queue admission.
    #[inline]
    pub fn queue(task_spec: TaskSpec) -> Self {
        Self::new(AdmissionPolicy::Queue, task_spec)
    }

    #[inline]
    pub fn replace(task_spec: TaskSpec) -> Self {
        Self::new(AdmissionPolicy::Replace, task_spec)
    }

    #[inline]
    pub fn drop_if_running(task_spec: TaskSpec) -> Self {
        Self::new(AdmissionPolicy::DropIfRunning, task_spec)
    }
}
