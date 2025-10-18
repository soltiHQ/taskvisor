use crate::TaskSpec;

use super::admission::Admission;

/// # ControllerSpec — logical request handled by the controller
///
/// Defines how the controller should treat the request for the given task.
#[derive(Clone)]
pub struct ControllerSpec {
    pub admission: Admission,
    pub spec: TaskSpec,
    name: String,
}

impl ControllerSpec {
    /// Create a new controller spec with admission policy and task spec.
    pub fn new(admission: Admission, spec: TaskSpec) -> Self {
        let name = spec.task().name().to_string();
        Self { admission, spec, name }
    }

    /// Slot name (derived from TaskSpec::task().name()).
    #[inline]
    pub fn name(&self) -> &str { &self.name }
}