//! Controller submission specification.
//!
//! [`ControllerSpec`] is the value passed to controller submission methods.
//! It combines:
//! - an [`AdmissionPolicy`], which says what to do when the target slot is busy,
//! - a [`TaskSpec`], which says what task to run and how it should run,
//! - an optional slot key, which groups submissions into one sequential lane.
//!
//! ## Slot vs Task Name
//!
//! The task name belongs to the runtime registry.
//! At most one registered task may use the same task name.
//!
//! The slot belongs to the controller.
//! At most one task may occupy the same slot at a time.
//!
//! If no slot is set, the slot defaults to the task name.
//! Use [`ControllerSpec::with_slot`] when several differently named tasks should share one admission lane.
//!
//! ```text
//! task name: "deploy-main-42"  ┐
//! task name: "deploy-main-43"  ├── slot: "deploy-main"
//! task name: "deploy-main-44"  ┘
//! ```
//!
//! All three tasks have different runtime names, but the controller admits them through one slot.

use std::sync::Arc;

use super::admission::AdmissionPolicy;
use crate::TaskSpec;

/// A request to submit one task through the controller.
///
/// A `ControllerSpec` does not run a task by itself.
/// It describes one submission: what task should be submitted, which admission policy should be used, and which slot should receive it.
///
/// The slot is admission metadata.
/// It is not part of the task execution model.
/// The task still runs according to its [`TaskSpec`].
///
/// # Example
///
/// ```rust
/// use taskvisor::{ControllerSpec, TaskContext, TaskError, TaskFn, TaskRef, TaskSpec};
///
/// let task: TaskRef = TaskFn::arc("deploy-main-42", |_ctx| async {
///     Ok(())
/// });
///
/// let spec = ControllerSpec::queue(TaskSpec::once(task)).with_slot("deploy-main");
///
/// assert_eq!(spec.slot_name(), "deploy-main");
/// ```
///
/// # Also
///
/// - [`AdmissionPolicy`] - how concurrent submissions to the same slot are handled
/// - [`TaskSpec`](crate::TaskSpec) - task restart, backoff, timeout, and retry settings
/// - [`SupervisorHandle::submit`](crate::SupervisorHandle::submit) - submit without waiting for the final outcome
/// - [`SupervisorHandle::submit_and_watch`](crate::SupervisorHandle::submit_and_watch) - submit and receive a [`TaskOutcome`](crate::TaskOutcome)
#[derive(Clone)]
#[must_use]
pub struct ControllerSpec {
    /// Admission policy used when the target slot is busy.
    admission: AdmissionPolicy,

    /// Task execution specification.
    ///
    /// The task name inside this spec remains the runtime task name.
    /// The controller slot may be the same name, or a separate grouping key set with [`with_slot`](Self::with_slot).
    task_spec: TaskSpec,

    /// Admission slot key.
    ///
    /// `None` means "use the task name".
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
    /// Creates a submission spec from an admission policy and a task spec.
    ///
    /// The slot defaults to the task name.
    /// Use [`with_slot`](Self::with_slot) to group several task names under one slot.
    pub fn new(admission: AdmissionPolicy, task_spec: TaskSpec) -> Self {
        Self {
            admission,
            task_spec,
            slot: None,
        }
    }

    /// Returns the admission policy used when the target slot is busy.
    #[must_use]
    pub fn admission(&self) -> AdmissionPolicy {
        self.admission
    }

    /// Sets the admission policy used when the target slot is busy.
    pub fn with_admission(mut self, admission: AdmissionPolicy) -> Self {
        self.admission = admission;
        self
    }

    /// Returns the task execution specification.
    #[must_use = "use the returned task specification"]
    pub fn task_spec(&self) -> &TaskSpec {
        &self.task_spec
    }

    /// Replaces the task execution specification.
    ///
    /// When no explicit slot is configured, the effective slot changes to the
    /// name of the replacement task. An explicit slot is preserved.
    pub fn with_task_spec(mut self, task_spec: TaskSpec) -> Self {
        self.task_spec = task_spec;
        self
    }

    /// Consumes this submission spec and returns its task execution specification.
    ///
    /// Controller-only metadata such as the admission policy and slot override is discarded.
    pub fn into_task_spec(self) -> TaskSpec {
        self.task_spec
    }

    /// Sets the admission slot key.
    ///
    /// The slot is the controller's concurrency unit: only one task can occupy a slot at a time.
    ///
    /// Use the same slot for work that must not run in parallel.
    /// Use different slots for work that may run independently.
    pub fn with_slot(mut self, slot: impl Into<Arc<str>>) -> Self {
        self.slot = Some(slot.into());
        self
    }

    /// Removes an explicit slot override.
    ///
    /// After this call, [`slot_name`](Self::slot_name) falls back to the current
    /// task name.
    pub fn without_slot(mut self) -> Self {
        self.slot = None;
        self
    }

    /// Returns the effective slot key.
    ///
    /// If [`with_slot`](Self::with_slot) was not used, this returns the task name from [`TaskSpec::name`](crate::TaskSpec::name).
    #[must_use]
    pub fn slot_name(&self) -> &str {
        self.slot
            .as_deref()
            .unwrap_or_else(|| self.task_spec.name())
    }

    /// Returns the explicit slot override, if one was configured.
    ///
    /// Unlike [`slot_name`](Self::slot_name), this does not fall back to the task name.
    #[must_use]
    pub fn slot_override(&self) -> Option<&str> {
        self.slot.as_deref()
    }

    /// Creates a submission with FIFO queue admission.
    ///
    /// If the slot is idle, the task is admitted immediately.
    /// If the slot is busy, the submission is queued behind older queued submissions.
    pub fn queue(task_spec: TaskSpec) -> Self {
        Self::new(AdmissionPolicy::Queue, task_spec)
    }

    /// Creates a latest-wins submission.
    ///
    /// If the slot is idle, the task is admitted immediately.
    /// If the slot is busy, the controller retires the current owner and keeps this submission as the next one to run.
    ///
    /// Repeated `Replace` submissions replace the next queued owner instead of growing the queue.
    pub fn replace(task_spec: TaskSpec) -> Self {
        Self::new(AdmissionPolicy::Replace, task_spec)
    }

    /// Creates a submit-if-idle submission.
    ///
    /// If the slot is idle, the task is admitted.
    /// If the slot is busy, the submission is rejected instead of queued.
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
            ControllerSpec::queue(make_spec("t")).admission(),
            AdmissionPolicy::Queue
        );
        assert_eq!(
            ControllerSpec::replace(make_spec("t")).admission(),
            AdmissionPolicy::Replace
        );
        assert_eq!(
            ControllerSpec::drop_if_running(make_spec("t")).admission(),
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
        assert_eq!(cs.slot_override(), Some("web"));
        assert_eq!(cs.task_spec().name(), "runner-web-7");
    }

    #[test]
    fn without_slot_restores_task_name_fallback() {
        let cs = ControllerSpec::queue(make_spec("runner-web-7"))
            .with_slot("web")
            .without_slot();

        assert_eq!(cs.slot_override(), None);
        assert_eq!(cs.slot_name(), "runner-web-7");
    }

    #[test]
    fn builders_replace_private_components() {
        let cs = ControllerSpec::queue(make_spec("old"))
            .with_slot("shared")
            .with_admission(AdmissionPolicy::Replace)
            .with_task_spec(make_spec("new"));

        assert_eq!(cs.admission(), AdmissionPolicy::Replace);
        assert_eq!(cs.task_spec().name(), "new");
        assert_eq!(cs.slot_name(), "shared");
        assert_eq!(cs.into_task_spec().name(), "new");
    }
}
