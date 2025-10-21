use super::admission::Admission;
use crate::TaskSpec;

/// Request to submit a task to the controller.
///
/// Combines a slot name, admission policy, and the actual task specification.
#[derive(Clone)]
pub struct ControllerSpec {
    /// Admission policy.
    pub admission: Admission,

    /// Task specification to run.
    pub task_spec: TaskSpec,
}

impl ControllerSpec {
    /// Creates a new controller submission specification.
    ///
    /// ## Parameters
    /// - `admission`: How to handle concurrent submissions
    /// - `task_spec`: The task to execute
    pub fn new(admission: Admission, task_spec: TaskSpec) -> Self {
        Self {
            admission,
            task_spec,
        }
    }

    /// Returns the slot name.
    pub fn slot_name(&self) -> &str {
        self.task_spec.name()
    }
}
