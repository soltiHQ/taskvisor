//! Builds one keyed admission request for the controller.
//!
//! [`ControllerSpec`] combines the work described by [`TaskSpec`] with a slot and a busy-slot [`AdmissionPolicy`].
//! [`SupervisorHandle::submit`](crate::SupervisorHandle::submit) consumes this value.
//! [`SupervisorHandle::add`](crate::SupervisorHandle::add) accepts `TaskSpec` instead.
//! Direct adds bypass keyed admission.
//!
//! See the [`controller`](crate::controller) module for the complete user flow.

use std::sync::Arc;

use super::policy::AdmissionPolicy;
use crate::TaskSpec;

/// A task, admission slot, and busy-slot policy submitted as one request.
///
/// The contained [`TaskSpec`] defines how the task runs after registry admission.
/// The controller settings define when it may enter the registry:
///
/// - an [`AdmissionPolicy`] for a busy slot;
/// - an optional slot that groups work which must not overlap.
///
/// Pass the request to [`SupervisorHandle::submit`](crate::SupervisorHandle::submit).
/// Add `watch()` before the terminal operation to observe rejection or the final task outcome.
/// Use the default unwatched operation when command intake alone is enough.
/// Allocate its task ID before intake with [`SupervisorHandle::prepare_submission`](crate::SupervisorHandle::prepare_submission).
///
/// # Admission flow
///
/// ```text
/// application
///      │ ControllerSpec
///      ▼
/// SupervisorHandle::submit
///      ▼
/// controller command queue
///      ▼
/// controller slot
///      ├── idle ──► TaskSpec ──► runtime registry ──► managed task
///      └── busy ──► apply AdmissionPolicy
/// ```
///
/// # Task name and slot
///
/// A task name is a unique registry key.
/// A slot groups work for controller admission.
/// Different task names can share a slot.
/// Without an explicit slot, the task name is used for both roles.
///
/// Slots do not create a second task namespace.
/// A task name stays reserved while the registry owns it.
/// Cleanup of a force-aborted task can also retain the name.
/// The name can be admitted again after that ownership ends.
///
/// # Examples
///
/// ```rust
/// use taskvisor::{AdmissionPolicy, ControllerSpec, TaskFn, TaskRef, TaskSpec};
///
/// let task: TaskRef = TaskFn::arc(|_ctx| async {
///     Ok(())
/// });
///
/// let request = ControllerSpec::queue(TaskSpec::once("deploy-main-42", task))
///     .with_slot("deploy-main");
///
/// assert_eq!(request.admission(), AdmissionPolicy::Queue);
/// assert_eq!(request.task_spec().name(), "deploy-main-42");
/// assert_eq!(request.slot_name(), "deploy-main");
/// ```
#[derive(Clone)]
#[must_use]
pub struct ControllerSpec {
    /// Policy applied when the effective slot has an owner.
    admission: AdmissionPolicy,

    /// Task passed to the runtime registry after controller admission.
    task_spec: TaskSpec,

    /// Explicit admission slot.
    ///
    /// `None` groups the submission by task name.
    slot: Option<Arc<str>>,
}

impl std::fmt::Debug for ControllerSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let slot = self.slot.as_deref().unwrap_or("<task-name>");
        f.debug_struct("ControllerSpec")
            .field("admission", &self.admission)
            .field("task_spec", &self.task_spec)
            .field("slot", &slot)
            .finish()
    }
}

impl ControllerSpec {
    /// Controller request from an explicit policy and task specification.
    ///
    /// The task name is the effective slot until [`with_slot`](Self::with_slot) sets a separate key.
    /// Prefer [`queue`](Self::queue), [`replace`](Self::replace), or
    /// [`drop_if_running`](Self::drop_if_running) when the policy is known at the call site.
    pub fn new(admission: AdmissionPolicy, task_spec: TaskSpec) -> Self {
        Self {
            admission,
            task_spec,
            slot: None,
        }
    }

    /// Busy-slot admission policy.
    #[must_use]
    pub fn admission(&self) -> AdmissionPolicy {
        self.admission
    }

    /// Busy-slot policy replacement that preserves the task and slot.
    pub fn with_admission(mut self, admission: AdmissionPolicy) -> Self {
        self.admission = admission;
        self
    }

    /// Borrowed runtime task specification.
    #[must_use = "use the returned task specification"]
    pub fn task_spec(&self) -> &TaskSpec {
        &self.task_spec
    }

    /// Runtime task-spec replacement that preserves any explicit slot.
    ///
    /// Without one, [`slot_name`](Self::slot_name) uses the name of the new task.
    pub fn with_task_spec(mut self, task_spec: TaskSpec) -> Self {
        self.task_spec = task_spec;
        self
    }

    /// Runtime task specification without controller settings.
    ///
    /// Pass this value to [`SupervisorHandle::add`](crate::SupervisorHandle::add) to bypass slot admission.
    pub fn into_task_spec(self) -> TaskSpec {
        self.task_spec
    }

    /// Explicit admission key separate from the task name.
    ///
    /// Tasks with the same effective slot cannot own that slot together.
    /// The task name remains unchanged and is still checked by the runtime registry.
    pub fn with_slot(mut self, slot: impl Into<Arc<str>>) -> Self {
        self.slot = Some(slot.into());
        self
    }

    /// Task-name admission with no explicit slot override.
    pub fn without_slot(mut self) -> Self {
        self.slot = None;
        self
    }

    /// Effective admission key from the explicit slot or task name.
    #[must_use]
    pub fn slot_name(&self) -> &str {
        self.slot
            .as_deref()
            .unwrap_or_else(|| self.task_spec.name())
    }

    /// Explicit slot override when configured.
    #[must_use]
    pub fn slot_override(&self) -> Option<&str> {
        self.slot.as_deref()
    }

    pub(crate) fn shared_slot_override(&self) -> Option<Arc<str>> {
        self.slot.as_ref().map(Arc::clone)
    }

    /// FIFO admission behind older work in a busy slot.
    ///
    /// Use this when incoming work should join the FIFO order.
    /// Queue limits can still reject the submission after command intake.
    /// See [`AdmissionPolicy::Queue`] for the full contract.
    pub fn queue(task_spec: TaskSpec) -> Self {
        Self::new(AdmissionPolicy::Queue, task_spec)
    }

    /// Newest-head admission in a busy slot.
    ///
    /// Use this when the next item should carry the newest value.
    /// This retires the current owner but does not clear older FIFO entries behind the head.
    /// See [`AdmissionPolicy::Replace`] for the full contract.
    pub fn replace(task_spec: TaskSpec) -> Self {
        Self::new(AdmissionPolicy::Replace, task_spec)
    }

    /// Idle-only admission for incoming work.
    ///
    /// A busy slot rejects the request without starting its task body.
    /// Add `watch()` to the submission operation when the caller must observe that decision.
    /// See [`AdmissionPolicy::DropIfRunning`] for the full contract.
    pub fn drop_if_running(task_spec: TaskSpec) -> Self {
        Self::new(AdmissionPolicy::DropIfRunning, task_spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::TaskContext;
    use crate::{TaskFn, TaskRef};

    fn make_spec(name: &str) -> TaskSpec {
        let task: TaskRef = TaskFn::arc(|_ctx: TaskContext| async { Ok(()) });
        TaskSpec::once(name, task)
    }

    #[test]
    fn convenience_constructors_set_correct_policy() {
        for (spec, expected) in [
            (
                ControllerSpec::queue(make_spec("queue")),
                AdmissionPolicy::Queue,
            ),
            (
                ControllerSpec::replace(make_spec("replace")),
                AdmissionPolicy::Replace,
            ),
            (
                ControllerSpec::drop_if_running(make_spec("drop")),
                AdmissionPolicy::DropIfRunning,
            ),
        ] {
            assert_eq!(spec.admission(), expected);
        }
    }

    #[test]
    fn slot_name_falls_back_to_task_name() {
        let cs = ControllerSpec::queue(make_spec("my-slot"));
        assert_eq!(cs.slot_name(), "my-slot");
    }

    #[test]
    fn slot_name_uses_explicit_slot() {
        let slot: Arc<str> = Arc::from("web");
        let cs = ControllerSpec::queue(make_spec("runner-web-7")).with_slot(Arc::clone(&slot));
        assert_eq!(cs.slot_name(), "web");
        assert_eq!(cs.slot_override(), Some("web"));
        assert_eq!(cs.task_spec().name(), "runner-web-7");
        assert!(Arc::ptr_eq(
            &slot,
            &cs.shared_slot_override().expect("explicit slot must exist")
        ));
    }

    #[test]
    fn debug_keeps_an_implicit_slot_symbolic() {
        let spec = ControllerSpec::queue(make_spec("debug-name"));
        let rendered = format!("{spec:?}");
        assert!(rendered.contains("<task-name>"));
    }
}
