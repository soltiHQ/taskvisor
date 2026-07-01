//! # Task completion outcomes.
//!
//! This module defines:
//! - [`TaskOutcome`]: the final result of one supervised task run,
//! - [`TaskWaiter`]: an awaitable receiver for that result.
//!
//! Outcomes are delivered on the completion plane, not through the event bus.
//! This means they are not affected by broadcast lag or dropped subscriber events.
//!
//! ## Completion Plane
//!
//! ```text
//! add_and_watch(spec)
//!   creates oneshot
//!   sends Add(id, spec, done) to registry
//!   returns TaskWaiter after registration is confirmed
//!
//! submit_and_watch(spec)
//!   creates oneshot
//!   parks sender in controller until admission or rejection
//!   returns TaskWaiter immediately after controller accepts the submission
//!
//! actor finishes
//!   registry joins actor
//!   registry maps actor exit to TaskOutcome
//!   registry resolves oneshot
//! ```
//!
//! ## Resolution Rules
//!
//! - One waiter observes one task identity.
//! - The outcome is resolved through a `oneshot` channel.
//! - The sender has one owner at a time.
//! - Dropping [`TaskWaiter`] is safe. Sending the outcome then becomes a no-op.
//! - The outcome describes the final actor result, after retries are finished.
//! - Per-attempt progress is reported by lifecycle events.
//!
//! ## Rejection
//!
//! [`TaskOutcome::Rejected`] means the task body never ran.
//!
//! It is normally observed from `submit_and_watch` (feature = `controller`), when the controller rejects a submission before it becomes a running task.
//! A duplicate-name registration failure is also resolved as `Rejected` inside the registry, but direct [`add_and_watch`](crate::SupervisorHandle::add_and_watch)
//! returns [`RuntimeError::TaskAlreadyExists`](crate::RuntimeError::TaskAlreadyExists) before handing the waiter to the caller.

use std::sync::Arc;

use tokio::sync::oneshot;

use crate::error::{RuntimeError, SharedError};
use crate::identity::TaskId;

/// Final result of a supervised task run.
///
/// This is the authoritative terminal result for a watched task.
/// It is delivered after the actor retry loop has ended and the registry has joined the actor.
///
/// Use events for live progress.
/// Use `TaskOutcome` when you need the final result.
///
/// ## Outcome vs Events
///
/// Events are best-effort observability.
/// They may be dropped under load, and one event kind can represent several final outcomes.
/// For example,
/// `ActorExhausted` can mean success, retry exhaustion, or task-reported cancellation depending on its reason.
/// `TaskOutcome` is delivered through a `oneshot` channel and is not affected by event-bus lag.
///
/// | Outcome                              | Typical event path | Meaning |
/// |--------------------------------------|------------------------------------------------------|------------------------------------------------------|
/// | [`Completed`](Self::Completed)       | `TaskStopped`, `ActorExhausted`, `TaskRemoved`       | success under `Never` or `OnFailure`                 |
/// | [`Failed`](Self::Failed)             | `TaskFailed`, `ActorExhausted`, `TaskRemoved`        | retryable/non-fatal error stopped after policy limit |
/// | [`Fatal`](Self::Fatal)               | `TaskFailed`, `ActorDead`, `TaskRemoved`             | fatal task error stopped immediately                 |
/// | [`Canceled`](Self::Canceled)         | `TaskCanceled`, `TaskRemoved`                        | cooperative cancellation                             |
/// | [`ForceAborted`](Self::ForceAborted) | `TaskRemoved(reason="force_terminated_after_grace")` | task did not stop in time                            |
/// | [`Panicked`](Self::Panicked)         | `ActorDead(reason="actor_panic")`, `TaskRemoved`     | actor task panicked                                  |
/// | [`Rejected`](Self::Rejected)         | `ControllerRejected` or `TaskAddFailed`              | task body never ran                                  |
///
/// # Also
///
/// - [`SupervisorHandle::add_and_watch`](crate::SupervisorHandle::add_and_watch) - direct watched task add
#[cfg_attr(
    feature = "controller",
    doc = "- [`SupervisorHandle::submit_and_watch`](crate::SupervisorHandle::submit_and_watch) - controller watched submission"
)]
/// - [`TaskWaiter`] - awaitable handle that returns this outcome
/// - [`EventKind`](crate::EventKind) - live observability events
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum TaskOutcome {
    /// Final attempt succeeded and restart policy stopped the actor.
    Completed,

    /// Final attempt failed and the actor stopped without a fatal error.
    ///
    /// Occurs when:
    /// - `RestartPolicy::Never` does not allow a retry,
    /// - the error is not retryable,
    /// - the retry budget is used up.
    #[non_exhaustive]
    Failed {
        /// Final failure message. Same text as the `ActorExhausted` event reason.
        reason: Arc<str>,
        /// Numeric exit code from a process-like task, if any.
        exit_code: Option<i32>,
        /// Original error source from the final [`TaskError`](crate::TaskError), if any.
        source: Option<SharedError>,
    },

    /// Task returned [`TaskError::Fatal`](crate::TaskError::Fatal).
    ///
    /// Fatal errors are not retried.
    #[non_exhaustive]
    Fatal {
        /// Fatal error message. Same text as the `ActorDead` event reason.
        reason: Arc<str>,
        /// Numeric exit code from a process-like task, if any.
        exit_code: Option<i32>,
        /// Original error source from the fatal [`TaskError`](crate::TaskError), if any.
        source: Option<SharedError>,
    },

    /// Task stopped because of cooperative cancellation.
    ///
    /// This can come from shutdown, explicit remove/cancel, or the task itself returning [`TaskError::Canceled`](crate::TaskError::Canceled).
    Canceled,

    /// Task ignored cooperative cancellation and was aborted after the grace period.
    ForceAborted,

    /// The actor itself panicked.
    ///
    /// This is a runtime bug guard.
    /// Panics inside the task body are caught by `run_once` and become retryable task failures instead.
    Panicked,

    /// The task body never ran.
    ///
    /// Common reasons:
    /// - controller slot was busy under `DropIfRunning`,
    /// - controller queue was full,
    /// - queued submission was replaced,
    /// - queued submission was removed,
    /// - controller was shutting down,
    /// - registration failed because the task name already existed.
    Rejected {
        /// Why the submission was rejected.
        reason: Arc<str>,
    },
}

impl TaskOutcome {
    /// Returns `true` only for [`Completed`](Self::Completed).
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, TaskOutcome::Completed)
    }

    /// Returns the original error source for [`Failed`](Self::Failed) or [`Fatal`](Self::Fatal).
    ///
    /// Returns `None` when the outcome has no source error.
    /// This allows callers to use `downcast_ref` or error-reporting crates on the completion plane.
    #[must_use]
    pub fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TaskOutcome::Failed { source, .. } | TaskOutcome::Fatal { source, .. } => {
                source.as_ref().map(|e| {
                    let e: &(dyn std::error::Error + 'static) = e.as_ref();
                    e
                })
            }
            _ => None,
        }
    }

    /// Returns a stable machine-readable label.
    ///
    /// Useful for logs, metrics, and telemetry.
    #[must_use]
    pub fn as_label(&self) -> &'static str {
        match self {
            TaskOutcome::Completed => "outcome_completed",
            TaskOutcome::Failed { .. } => "outcome_failed",
            TaskOutcome::Fatal { .. } => "outcome_fatal",
            TaskOutcome::Canceled => "outcome_canceled",
            TaskOutcome::ForceAborted => "outcome_force_aborted",
            TaskOutcome::Panicked => "outcome_panicked",
            TaskOutcome::Rejected { .. } => "outcome_rejected",
        }
    }
}

/// Awaitable handle for one task outcome.
///
/// Created by:
/// - [`SupervisorHandle::add_and_watch`](crate::SupervisorHandle::add_and_watch)
#[cfg_attr(
    feature = "controller",
    doc = "- [`SupervisorHandle::submit_and_watch`](crate::SupervisorHandle::submit_and_watch)"
)]
///
/// A waiter is consumed by [`wait`](Self::wait).
/// It resolves once the watched task reaches a terminal outcome, or returns an error if the sender is dropped before an outcome is produced.
///
/// ## Example
///
/// ```rust,no_run
/// # use std::time::Duration;
/// # use taskvisor::prelude::*;
/// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
/// # let handle = sup.serve();
/// let job: TaskRef = TaskFn::arc("job", |_ctx: TaskContext| async {
///     Ok::<(), TaskError>(())
/// });
///
/// let (id, waiter) = handle
///     .add_and_watch(TaskSpec::once(job), Duration::from_secs(1))
///     .await?;
///
/// match waiter.wait().await? {
///     TaskOutcome::Completed => println!("{id} completed"),
///     other => eprintln!("{id} ended with {other:?}"),
/// }
/// # Ok(()) }
/// ```
#[derive(Debug)]
#[must_use = "a TaskWaiter does nothing unless awaited via `.wait()`"]
pub struct TaskWaiter {
    id: TaskId,
    rx: oneshot::Receiver<TaskOutcome>,
}

impl TaskWaiter {
    /// Creates a waiter for one task identity.
    pub(crate) fn new(id: TaskId, rx: oneshot::Receiver<TaskOutcome>) -> Self {
        Self { id, rx }
    }

    /// Returns the task identity observed by this waiter.
    #[must_use]
    pub fn id(&self) -> TaskId {
        self.id
    }

    /// Waits until the task reaches a final outcome.
    ///
    /// During normal operation, this returns a [`TaskOutcome`].
    /// During normal shutdown, tasks resolve to [`TaskOutcome::Canceled`] or [`TaskOutcome::ForceAborted`].
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::ShuttingDown`] if the runtime drops the sending half before producing an outcome.
    /// This means the watched task was never resolved by the registry/controller path.
    pub async fn wait(self) -> Result<TaskOutcome, RuntimeError> {
        self.rx.await.map_err(|_| RuntimeError::ShuttingDown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_stable_and_distinct() {
        let outcomes = [
            TaskOutcome::Completed,
            TaskOutcome::Failed {
                reason: Arc::from("x"),
                exit_code: None,
                source: None,
            },
            TaskOutcome::Fatal {
                reason: Arc::from("x"),
                exit_code: Some(1),
                source: None,
            },
            TaskOutcome::Canceled,
            TaskOutcome::ForceAborted,
            TaskOutcome::Panicked,
            TaskOutcome::Rejected {
                reason: Arc::from("x"),
            },
        ];
        let labels: std::collections::HashSet<&str> =
            outcomes.iter().map(|o| o.as_label()).collect();
        assert_eq!(labels.len(), outcomes.len(), "labels must be distinct");
    }

    #[test]
    fn only_completed_is_success() {
        assert!(TaskOutcome::Completed.is_success());
        assert!(!TaskOutcome::Canceled.is_success());
        assert!(!TaskOutcome::Panicked.is_success());
    }

    #[test]
    fn failed_outcome_exposes_downcastable_source() {
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let outcome = TaskOutcome::Failed {
            reason: Arc::from("denied"),
            exit_code: None,
            source: Some(Arc::new(io)),
        };

        let src = outcome
            .source()
            .expect("a Failed outcome with a cause must expose its source");
        assert_eq!(
            src.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn sourceless_outcomes_report_no_source() {
        assert!(TaskOutcome::Completed.source().is_none());
        assert!(
            TaskOutcome::Failed {
                reason: Arc::from("plain"),
                exit_code: Some(1),
                source: None,
            }
            .source()
            .is_none()
        );
    }

    #[tokio::test]
    async fn waiter_resolves_with_sent_outcome() {
        let (tx, rx) = oneshot::channel();
        let waiter = TaskWaiter::new(TaskId::next(), rx);
        tx.send(TaskOutcome::Completed).unwrap();
        assert!(matches!(
            waiter.wait().await.unwrap(),
            TaskOutcome::Completed
        ));
    }

    #[tokio::test]
    async fn waiter_maps_dropped_sender_to_shutting_down() {
        let (tx, rx) = oneshot::channel::<TaskOutcome>();
        let waiter = TaskWaiter::new(TaskId::next(), rx);
        drop(tx);
        assert!(matches!(
            waiter.wait().await,
            Err(RuntimeError::ShuttingDown)
        ));
    }
}
