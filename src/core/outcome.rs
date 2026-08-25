//! Delivers the final result of watched work outside the best-effort event bus.
//!
//! A [`TaskWaiter`] follows one [`TaskId`] through a direct one-shot channel. For admitted work,
//! the registry delivers a [`TaskOutcome`] after the managed actor produces a terminal result
//! and registry membership is removed. Controller submissions can instead resolve
//! as [`TaskOutcome::Rejected`] before the task body starts.
//!
//! ```text
//! watched work
//!      ├── controller rejection ──► TaskOutcome::Rejected
//!      └── registry admission ──► TaskActor ──► terminal registry commit
//!                                                     │ TaskOutcome
//!                                                     ▼
//!                                                 TaskWaiter
//! ```
//!
//! Except for [`TaskOutcome::ForceAborted`], the registry joins the managed actor before delivering the outcome.
//! A force-aborted actor can remain physically active after the waiter resolves. Dropping the waiter does not
//! cancel the work. Final destruction of the retained task object happens later on deferred-cleanup workers.
//! A panic during that later destruction is a runtime diagnostic and cannot revise an outcome already delivered.
//! This path is reliable while the process and runtime are alive; it is not durable storage across process termination.

use std::sync::Arc;

use tokio::sync::oneshot;

use crate::error::{RuntimeError, SharedError};
use crate::events::RejectionKind;
use crate::identity::TaskId;

/// Machine-readable category of a final [`TaskOutcome`].
///
/// It mirrors [`TaskOutcome`] without diagnostic text or source errors.
/// Events use it for machine-readable reporting.
/// Use [`TaskOutcome::kind`] when the complete outcome is already available.
///
/// Match with a wildcard arm because new outcome categories may be added.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TaskOutcomeKind {
    /// Final attempt succeeded and policy stopped the task.
    Completed,
    /// A non-fatal failure reached a policy or retry-limit stop condition.
    Failed,
    /// The task reported a permanent failure.
    Fatal,
    /// Cancellation was requested or reported cooperatively.
    Canceled,
    /// Taskvisor requested abort after cooperative cancellation did not complete.
    ForceAborted,
    /// The task actor or protected attempt-owned cleanup panicked before terminal outcome delivery.
    Panicked,
    /// Admission rejected the work before its task body ran.
    Rejected,
}

impl TaskOutcomeKind {
    /// Returns the stable machine-readable label used by events, logs, and metrics.
    #[must_use]
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::Completed => "outcome_completed",
            Self::Failed => "outcome_failed",
            Self::Fatal => "outcome_fatal",
            Self::Canceled => "outcome_canceled",
            Self::ForceAborted => "outcome_force_aborted",
            Self::Panicked => "outcome_panicked",
            Self::Rejected => "outcome_rejected",
        }
    }
}

/// Final classified result of one watched task or controller submission.
///
/// Admitted work receives this value after terminal registry cleanup removes its membership.
/// Except for [`ForceAborted`](Self::ForceAborted), cleanup first joins the managed actor.
/// Controller admission can produce [`Rejected`](Self::Rejected) without running the task body.
///
/// This enum and its data-carrying variants are non-exhaustive.
/// Use a fallback arm and `..` when matching fields.
///
/// Watched work is created by:
/// - [`SupervisorHandle::add_and_watch`](crate::SupervisorHandle::add_and_watch)
/// - [`SupervisorHandle::try_add_and_watch`](crate::SupervisorHandle::try_add_and_watch)
#[cfg_attr(
    feature = "controller",
    doc = "- [`SupervisorHandle::submit_and_watch`](crate::SupervisorHandle::submit_and_watch) and [`SupervisorHandle::try_submit_and_watch`](crate::SupervisorHandle::try_submit_and_watch) - controller watched submission"
)]
///
/// Events carry [`TaskOutcomeKind`] for best-effort observation.
/// A [`TaskWaiter`] delivers this complete value directly.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum TaskOutcome {
    /// Final attempt succeeded and the restart policy stopped the task.
    Completed,

    /// A retryable failure stopped under restart policy or the retry limit.
    #[non_exhaustive]
    Failed {
        /// Diagnostic final failure message.
        ///
        /// This text is not a machine-readable category and may change.
        /// Use [`TaskOutcome::kind`] for branching, metrics, and alerts.
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
        /// Diagnostic fatal error message.
        ///
        /// This text is not a machine-readable category and may change.
        /// Use [`TaskOutcome::kind`] for branching, metrics, and alerts.
        reason: Arc<str>,
        /// Numeric exit code from a process-like task, if any.
        exit_code: Option<i32>,
        /// Original error source from the fatal [`TaskError`](crate::TaskError), if any.
        source: Option<SharedError>,
    },

    /// Task stopped because cancellation was requested or reported.
    ///
    /// This can come from shutdown, explicit removal, or a returned [`TaskError::Canceled`](crate::TaskError::Canceled).
    Canceled,

    /// The registry stopped waiting and requested abort before cooperative stop completed.
    ///
    /// This normally happens after the configured grace period.
    /// Last-owner fallback and signal-setup failure cleanup cannot wait for that period.
    /// A synchronous poll can remain physically active until it returns control to Tokio.
    ForceAborted,

    /// The actor panicked, or dropping attempt-owned data inside the physical actor boundary panicked.
    ///
    /// A panic from task polling becomes a retryable task failure instead.
    /// A later panic while deferred cleanup destroys the retained task object does not change the outcome already delivered.
    Panicked,

    /// Controller or registry admission rejected the work before its task body ran.
    #[non_exhaustive]
    Rejected {
        /// Stable category for machine-readable handling.
        kind: RejectionKind,
        /// Readable diagnostic rejection details.
        ///
        /// Use `kind` instead of parsing this text.
        reason: Arc<str>,
    },
}

impl TaskOutcome {
    /// Returns the machine-readable category of this outcome.
    #[must_use]
    pub const fn kind(&self) -> TaskOutcomeKind {
        match self {
            TaskOutcome::Completed => TaskOutcomeKind::Completed,
            TaskOutcome::Failed { .. } => TaskOutcomeKind::Failed,
            TaskOutcome::Fatal { .. } => TaskOutcomeKind::Fatal,
            TaskOutcome::Canceled => TaskOutcomeKind::Canceled,
            TaskOutcome::ForceAborted => TaskOutcomeKind::ForceAborted,
            TaskOutcome::Panicked => TaskOutcomeKind::Panicked,
            TaskOutcome::Rejected { .. } => TaskOutcomeKind::Rejected,
        }
    }

    /// Returns whether the task reached [`Completed`](Self::Completed).
    ///
    /// Cancellation, rejection, and every failure category return `false`.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, TaskOutcome::Completed)
    }

    /// Creates a [`Failed`](Self::Failed) outcome for tests.
    ///
    /// This helper lets external tests construct the non-exhaustive variant.
    /// The source error is `None`.
    ///
    /// ```rust
    /// use taskvisor::TaskOutcome;
    ///
    /// let outcome = TaskOutcome::failed_for_tests("boom", Some(3));
    /// assert!(!outcome.is_success());
    /// ```
    #[cfg(feature = "test-util")]
    #[cfg_attr(docsrs, doc(cfg(feature = "test-util")))]
    #[must_use]
    pub fn failed_for_tests(reason: impl Into<Arc<str>>, exit_code: Option<i32>) -> Self {
        Self::Failed {
            reason: reason.into(),
            exit_code,
            source: None,
        }
    }

    /// Creates a [`Fatal`](Self::Fatal) outcome for tests.
    ///
    /// This helper lets external tests construct the non-exhaustive variant.
    /// The source error is `None`.
    #[cfg(feature = "test-util")]
    #[cfg_attr(docsrs, doc(cfg(feature = "test-util")))]
    #[must_use]
    pub fn fatal_for_tests(reason: impl Into<Arc<str>>, exit_code: Option<i32>) -> Self {
        Self::Fatal {
            reason: reason.into(),
            exit_code,
            source: None,
        }
    }

    /// Creates a [`Rejected`](Self::Rejected) outcome for tests.
    ///
    /// This helper lets external tests construct the non-exhaustive variant.
    #[cfg(feature = "test-util")]
    #[cfg_attr(docsrs, doc(cfg(feature = "test-util")))]
    #[must_use]
    pub fn rejected_for_tests(kind: RejectionKind, reason: impl Into<Arc<str>>) -> Self {
        Self::Rejected {
            kind,
            reason: reason.into(),
        }
    }

    /// Returns the original error source for [`Failed`](Self::Failed) or [`Fatal`](Self::Fatal).
    ///
    /// Returns `None` when the outcome has no source error.
    ///
    /// Callers can use `downcast_ref` or pass the source to an error reporter.
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
        self.kind().as_label()
    }
}

/// One-shot receiver that follows one identity to its final [`TaskOutcome`].
///
/// Created by:
/// - [`SupervisorHandle::add_and_watch`](crate::SupervisorHandle::add_and_watch)
/// - [`SupervisorHandle::try_add_and_watch`](crate::SupervisorHandle::try_add_and_watch)
#[cfg_attr(
    feature = "controller",
    doc = "- [`SupervisorHandle::submit_and_watch`](crate::SupervisorHandle::submit_and_watch)\n- [`SupervisorHandle::try_submit_and_watch`](crate::SupervisorHandle::try_submit_and_watch)\n- [`PreparedSubmission::submit_and_watch`](crate::PreparedSubmission::submit_and_watch)\n- [`PreparedSubmission::try_submit_and_watch`](crate::PreparedSubmission::try_submit_and_watch)"
)]
///
/// [`wait`](Self::wait) consumes the waiter. Dropping it does not cancel the task or submission.
/// Keep the waiter when application behavior depends on the result; use events only for best-effort observation.
///
/// # Examples
///
/// ```rust,no_run
/// # use taskvisor::prelude::*;
/// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
/// # let handle = sup.serve()?;
/// let job: TaskRef = TaskFn::arc(|_ctx| async {
///     Ok(())
/// });
///
/// let (id, waiter) = handle
///     .add_and_watch(TaskSpec::once("job", job))
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

    /// Returns the task or submission identity followed by this waiter.
    #[must_use]
    pub fn id(&self) -> TaskId {
        self.id
    }

    /// Waits for the final outcome.
    ///
    /// For admitted work, this normally resolves after registry membership is removed. Controller rejection
    /// can resolve without starting the task. Shutdown does not replace an outcome already owned by terminal cleanup.
    /// Resolution does not mean that deferred destruction of the retained task object has finished.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::OutcomeUnavailable`] if the sender closes before producing an outcome.
    pub async fn wait(self) -> Result<TaskOutcome, RuntimeError> {
        let id = self.id;
        self.rx
            .await
            .map_err(|_| RuntimeError::OutcomeUnavailable { id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_waiter_after_delivery_drops_caller_owned_outcome_locally() {
        use std::{
            fmt,
            sync::atomic::{AtomicBool, Ordering},
        };

        #[derive(Debug)]
        struct DropThreadProbe {
            dropped: Arc<AtomicBool>,
            isolated: Arc<AtomicBool>,
        }

        impl fmt::Display for DropThreadProbe {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("waiter drop probe")
            }
        }

        impl std::error::Error for DropThreadProbe {}

        impl Drop for DropThreadProbe {
            fn drop(&mut self) {
                let isolated = std::thread::current()
                    .name()
                    .is_some_and(|name| name.starts_with("taskvisor-drop-"));
                self.isolated.store(isolated, Ordering::Release);
                self.dropped.store(true, Ordering::Release);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let isolated = Arc::new(AtomicBool::new(false));
        let source: SharedError = Arc::new(DropThreadProbe {
            dropped: Arc::clone(&dropped),
            isolated: Arc::clone(&isolated),
        });
        let (tx, rx) = oneshot::channel();
        let waiter = TaskWaiter::new(TaskId::next(), rx);

        tx.send(TaskOutcome::Failed {
            reason: Arc::from("failed"),
            exit_code: None,
            source: Some(source),
        })
        .expect("the waiter receiver must be live");

        drop(waiter);
        assert!(
            dropped.load(Ordering::Acquire),
            "a delivered outcome belongs to the waiter caller"
        );
        assert!(
            !isolated.load(Ordering::Acquire),
            "the library reservation ends when outcome delivery succeeds"
        );
    }

    #[cfg(feature = "test-util")]
    #[test]
    fn test_constructors_build_the_terminal_failure_and_rejection_variants() {
        let failed = TaskOutcome::failed_for_tests("boom", Some(3));
        assert!(matches!(
            &failed,
            TaskOutcome::Failed { reason, exit_code: Some(3), .. } if reason.as_ref() == "boom"
        ));
        assert!(failed.source().is_none(), "test outcomes carry no source");

        let fatal = TaskOutcome::fatal_for_tests("bad config", None);
        assert!(matches!(
            &fatal,
            TaskOutcome::Fatal { reason, exit_code: None, .. } if reason.as_ref() == "bad config"
        ));

        let rejected = TaskOutcome::rejected_for_tests(
            RejectionKind::QueueFull,
            "slot queue reached capacity",
        );
        assert!(matches!(
            &rejected,
            TaskOutcome::Rejected { kind: RejectionKind::QueueFull, reason, .. }
                if reason.as_ref() == "slot queue reached capacity"
        ));
        assert!(rejected.source().is_none());
    }

    #[test]
    fn labels_and_success_flags_are_stable_for_every_variant() {
        let cases = [
            (
                TaskOutcome::Completed,
                TaskOutcomeKind::Completed,
                "outcome_completed",
                true,
            ),
            (
                TaskOutcome::Failed {
                    reason: Arc::from("x"),
                    exit_code: None,
                    source: None,
                },
                TaskOutcomeKind::Failed,
                "outcome_failed",
                false,
            ),
            (
                TaskOutcome::Fatal {
                    reason: Arc::from("x"),
                    exit_code: Some(1),
                    source: None,
                },
                TaskOutcomeKind::Fatal,
                "outcome_fatal",
                false,
            ),
            (
                TaskOutcome::Canceled,
                TaskOutcomeKind::Canceled,
                "outcome_canceled",
                false,
            ),
            (
                TaskOutcome::ForceAborted,
                TaskOutcomeKind::ForceAborted,
                "outcome_force_aborted",
                false,
            ),
            (
                TaskOutcome::Panicked,
                TaskOutcomeKind::Panicked,
                "outcome_panicked",
                false,
            ),
            (
                TaskOutcome::Rejected {
                    kind: RejectionKind::AdmissionFailed,
                    reason: Arc::from("x"),
                },
                TaskOutcomeKind::Rejected,
                "outcome_rejected",
                false,
            ),
        ];

        let labels: std::collections::HashSet<_> = cases
            .iter()
            .map(
                |(outcome, expected_kind, expected_label, expected_success)| {
                    assert_eq!(outcome.kind(), *expected_kind);
                    assert_eq!(outcome.as_label(), *expected_label);
                    assert_eq!(expected_kind.as_label(), *expected_label);
                    assert_eq!(outcome.is_success(), *expected_success, "{expected_label}");
                    outcome.as_label()
                },
            )
            .collect();
        assert_eq!(labels.len(), cases.len(), "labels must remain distinct");
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
    async fn waiter_resolves_sent_outcome_and_maps_a_dropped_sender() {
        let (tx, rx) = oneshot::channel();
        let waiter = TaskWaiter::new(TaskId::next(), rx);
        tx.send(TaskOutcome::Completed).unwrap();
        assert!(matches!(
            waiter.wait().await.unwrap(),
            TaskOutcome::Completed
        ));

        let (tx, rx) = oneshot::channel::<TaskOutcome>();
        let id = TaskId::next();
        let waiter = TaskWaiter::new(id, rx);
        drop(tx);
        assert!(matches!(
            waiter.wait().await,
            Err(RuntimeError::OutcomeUnavailable { id: unavailable }) if unavailable == id
        ));
    }
}
