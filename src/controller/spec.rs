use super::admission::Admission;
use crate::TaskSpec;

/// Request to submit a task to the controller.
///
/// Combines a slot name, admission policy, and the actual task specification.
#[derive(Clone)]
pub struct ControllerSpec {
    /// Logical slot name (tasks with same name share a slot).
    slot_name: String,

    /// Admission policy.
    pub admission: Admission,

    /// Task specification to run.
    pub task_spec: TaskSpec,
}

impl ControllerSpec {
    /// Creates a new controller submission specification.
    ///
    /// ## Parameters
    /// - `slot_name`: Logical slot identifier (tasks with same name share a slot)
    /// - `admission`: How to handle concurrent submissions
    /// - `task_spec`: The task to execute
    ///
    /// ## Note
    /// `slot_name` is independent of `task_spec.name()` — you can run the same
    /// task in different slots by using different slot names.
    pub fn new(slot_name: impl Into<String>, admission: Admission, task_spec: TaskSpec) -> Self {
        Self {
            slot_name: slot_name.into(),
            admission,
            task_spec,
        }
    }

    /// Returns the slot name.
    pub fn slot_name(&self) -> &str {
        &self.slot_name
    }
}
