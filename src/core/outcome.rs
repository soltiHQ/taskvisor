//! # Reliable final task results
//!
//! Lifecycle events answer "what is happening now?".
//! They may be dropped. Application logic must not reconstruct a final result from them.
//!
//! A [`TaskWaiter`] answers "how did this task end?" for one [`TaskId`].
//! It receives one final [`TaskOutcome`] through a direct one-shot channel, outside the event bus.
//! For an admitted task, the registry sends the outcome after joining its managed runner and removing registry membership.
//! Event-bus lag does not affect this path. If the outcome cannot be delivered, [`TaskWaiter::wait`] returns an error instead of guessing the result.
//!
//! ## Successful Direct-Add Flow
//!
//! ```text
//! Caller                         Runtime                      Managed runner
//!   │                               │                                │
//!   ├── add_and_watch(spec) ───────►│                                │
//!   │                               ├── register and spawn ─────────►│
//!   │◄──── (TaskId, TaskWaiter) ────┤                                │
//!   │                               │                                │ attempts / retries
//!   │ await waiter.wait()           │                                │
//!   │                               │◄─────── terminal signal ───────┤
//!   │                               │ join runner                    │
//!   │                               │ remove TaskId and name         │
//!   │◄──── TaskOutcome (oneshot) ───┤                                │
//! ```
//!
//! With the `controller` feature, `submit_and_watch` can also return a waiter before slot admission.
//! If the controller rejects the submission, the final outcome is [`TaskOutcome::Rejected`] and the task body never runs.
//!
//! ## Guarantees
//!
//! - One waiter follows one [`TaskId`].
//! - For admitted work, it resolves after all retries end and the registry joins the managed runner.
//! - Dropping a waiter is safe and does not cancel the task.
//! - If the runtime drops the sender before it creates an outcome, [`TaskWaiter::wait`] returns an error instead of inventing a result.
//!
//! Direct [`SupervisorHandle::add_and_watch`](crate::SupervisorHandle::add_and_watch)
//! returns registration errors such as a duplicate name before it gives the
//! caller a waiter.

use std::sync::Arc;

use tokio::sync::oneshot;

use crate::error::{RuntimeError, SharedError};
use crate::events::RejectionKind;
use crate::identity::TaskId;

/// Machine-readable category of a final [`TaskOutcome`].
///
/// This lightweight enum mirrors [`TaskOutcome`] without carrying diagnostic
/// text or source errors. It is used by lifecycle [`Event`](crate::Event)
/// values so metrics, dashboards, and alerts never need to parse `reason`.
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
    /// The runtime aborted the task before cooperative stop completed.
    ForceAborted,
    /// The internal task runner panicked.
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

/// Final result of one watched task or controller submission.
///
/// For admitted work, this value is sent after the retry loop ends, the managed runner is joined, and registry membership is removed.
/// A controller can instead return [`Rejected`](Self::Rejected) before the task starts.
///
/// This enum is non-exhaustive.
/// Include a fallback arm when matching it.
/// The data-carrying variants are also non-exhaustive. Match their fields with `..`.
///
/// ## Outcome vs Events
///
/// Events are best-effort and may be missing.
/// Do not rebuild a final outcome by collecting event kinds.
/// A waiter uses a separate, reliable runtime channel.
///
/// | Outcome                              | Meaning                                    |
/// |--------------------------------------|--------------------------------------------|
/// | [`Completed`](Self::Completed)       | Final attempt succeeded and policy stopped |
/// | [`Failed`](Self::Failed)             | Retryable failure reached a stop condition |
/// | [`Fatal`](Self::Fatal)               | Task reported a permanent failure          |
/// | [`Canceled`](Self::Canceled)         | Cooperative cancellation                   |
/// | [`ForceAborted`](Self::ForceAborted) | Runtime aborted before cooperative stop    |
/// | [`Panicked`](Self::Panicked)         | Internal task runner panicked              |
/// | [`Rejected`](Self::Rejected)         | Task body never ran                        |
///
/// ## See Also
///
/// - [`SupervisorHandle::add_and_watch`](crate::SupervisorHandle::add_and_watch) and
///   [`SupervisorHandle::try_add_and_watch`](crate::SupervisorHandle::try_add_and_watch) - direct watched task add
#[cfg_attr(
    feature = "controller",
    doc = "- [`SupervisorHandle::submit_and_watch`](crate::SupervisorHandle::submit_and_watch) and [`SupervisorHandle::try_submit_and_watch`](crate::SupervisorHandle::try_submit_and_watch) - controller watched submission"
)]
/// - [`TaskWaiter`] - awaitable handle that returns this outcome
/// - [`EventKind`](crate::EventKind) - live observability events
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum TaskOutcome {
    /// Final attempt succeeded and the restart policy stopped the task.
    Completed,

    /// A retryable failure reached a policy or retry-limit stop condition.
    ///
    /// Occurs when:
    /// - `RestartPolicy::Never` does not allow a retry,
    /// - the error is not retryable,
    /// - the retry budget is used up.
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
    /// This can come from shutdown, explicit removal, or the task returning [`TaskError::Canceled`](crate::TaskError::Canceled).
    Canceled,

    /// The runtime aborted the managed task runner before cooperative stop completed.
    ///
    /// This normally happens after the configured grace period.
    /// Last-owner fallback and signal-setup failure cleanup cannot wait for that period.
    ForceAborted,

    /// The internal task runner panicked.
    ///
    /// This guards against a runtime bug.
    /// Panics inside the user task are caught earlier and become retryable failures instead.
    Panicked,

    /// The task body never ran.
    ///
    /// For controller submissions, common reasons are:
    /// - controller slot was busy under `DropIfRunning`,
    /// - controller slot queue was full,
    /// - queued submission was replaced,
    /// - queued submission was removed,
    /// - controller was shutting down,
    /// - registration failed because the task name already existed.
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

    /// Returns `true` only for [`Completed`](Self::Completed).
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, TaskOutcome::Completed)
    }

    /// Creates a [`Failed`](Self::Failed) outcome for tests.
    ///
    /// Real outcomes normally come from the runtime.
    /// The `Failed`, `Fatal`, and `Rejected` variants are `#[non_exhaustive]`; other crates cannot build them directly.
    ///
    /// This helper lets tests cover code that handles failed outcomes; `source` is `None`.
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
    /// See [`failed_for_tests`](Self::failed_for_tests) for why this helper exists; `source` is `None`.
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
    /// See [`failed_for_tests`](Self::failed_for_tests) for why this helper exists.
    ///
    /// ```rust
    /// use taskvisor::TaskOutcome;
    ///
    /// let outcome = TaskOutcome::rejected_for_tests(
    ///     taskvisor::RejectionKind::QueueFull,
    ///     "slot queue reached capacity",
    /// );
    /// assert_eq!(outcome.as_label(), "outcome_rejected");
    /// ```
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
    /// > Callers can use `downcast_ref` or pass it to an error-reporting library.
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

/// One-shot receiver for a final [`TaskOutcome`].
///
/// Created by:
/// - [`SupervisorHandle::add_and_watch`](crate::SupervisorHandle::add_and_watch)
/// - [`SupervisorHandle::try_add_and_watch`](crate::SupervisorHandle::try_add_and_watch)
#[cfg_attr(
    feature = "controller",
    doc = "- [`SupervisorHandle::submit_and_watch`](crate::SupervisorHandle::submit_and_watch)\n- [`SupervisorHandle::try_submit_and_watch`](crate::SupervisorHandle::try_submit_and_watch)\n- [`PreparedSubmission::submit_and_watch`](crate::PreparedSubmission::submit_and_watch)\n- [`PreparedSubmission::try_submit_and_watch`](crate::PreparedSubmission::try_submit_and_watch)"
)]
///
/// [`wait`](Self::wait) consumes the waiter.
/// It normally resolves after the task or submission reaches a final outcome. Dropping the waiter does not cancel the task.
///
/// ## Example
///
/// ```rust,no_run
/// # use taskvisor::prelude::*;
/// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
/// # let handle = sup.serve();
/// let job: TaskRef = TaskFn::arc("job", |_ctx| async {
///     Ok(())
/// });
///
/// let (id, waiter) = handle
///     .add_and_watch(TaskSpec::once(job))
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
    /// A registered task stopped by shutdown normally resolves as [`TaskOutcome::Canceled`] or [`TaskOutcome::ForceAborted`].
    /// Work that was already finishing can keep its own terminal outcome.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::ShuttingDown`] if the runtime drops its sender before producing an outcome.
    ///
    /// > No final result is available in that case.
    pub async fn wait(self) -> Result<TaskOutcome, RuntimeError> {
        self.rx.await.map_err(|_| RuntimeError::ShuttingDown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let waiter = TaskWaiter::new(TaskId::next(), rx);
        drop(tx);
        assert!(matches!(
            waiter.wait().await,
            Err(RuntimeError::ShuttingDown)
        ));
    }
}
