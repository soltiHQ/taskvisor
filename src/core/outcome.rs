//! # Task completion outcomes and the awaitable [`TaskWaiter`].
//!
//! [`TaskOutcome`] is the **final** result of a supervised task run: it is produced exactly once,
//! after the actor's retry loop has finished and its `JoinHandle` has been joined by the registry.
//!
//! [`TaskWaiter`] is the receiving half of a `oneshot` channel created at
//! registration time by [`SupervisorHandle::add_and_watch`](crate::SupervisorHandle::add_and_watch).
//!
//! ## Architecture: one sender, resolved at exactly one site
//!
//! The `oneshot` sender is minted at registration and travels with the task until a single site resolves it.
//! There are two birth points (direct vs. controller) and a closed set of resolution sites - **every** terminal path resolves
//! the sender - waiter can never hang:
//!
//! ```text
//!  BIRTH                                  TRAVELS IN              RESOLVED AT (exactly one)
//!  ─────                                  ──────────              ─────────────────────────
//!  add_and_watch(spec) ───────────┐
//!    oneshot::channel()           ├─► RegistryCommand::Add ─► Registry Handle.done ─┐
//!    returns (id, TaskWaiter{rx}) │     (mpsc, guaranteed)                          │
//!                                 │                                                 ▼
//!  submit_and_watch(spec) ────────┘                              ┌─ report_join (cooperative join)
//!    oneshot::channel()                                          │    Ok(Completed)  → Completed
//!    Controller.watchers[id]=tx ──► admitted ─► start_in_slot ───┤    Ok(Exhausted)  → Failed
//!    (parked until admitted or rejected)       hands tx to Add   │    Ok(Fatal)      → Fatal
//!         │                                                      │    Ok(Canceled)   → Canceled
//!         │                                                      │    Err(panic)     → Panicked
//!         └─ never admitted ─► finalize_rejected ─► Rejected     └─ force-abort path → ForceAborted
//! ```
//!
//! ## Rules
//!
//! - The outcome is delivered via `oneshot` (guaranteed, not subject to bus `Lagged` loss).
//! - The channel is created **atomically with registration**: there is no window in which the task can finish before the waiter exists.
//! - **Exactly-once by construction**: the sender has a single owner (the registry `Handle`, or the controller `watchers` map before admission);
//!   ownership transfers to exactly one resolution site, so the waiter is resolved once and never leaks.
//! - Dropping a [`TaskWaiter`] is always safe: the matching `send` becomes a no-op.
//! - The outcome reflects the **final** attempt only; per-attempt results are observable via [`Subscribe`](crate::Subscribe) events.
//! - `Rejected` is observed only on the controller path: a submission that never ran - never admitted (slot busy, queue full, superseded, removed while queued, shutting down) or rejected at registration (duplicate task name) - resolves there.

use std::sync::Arc;

use tokio::sync::oneshot;

use crate::error::{RuntimeError, SharedError};
use crate::identity::TaskId;

/// Final result of a supervised task run.
///
/// Delivered exactly once per task, after the actor has fully terminated (retry loop finished **and** the actor's `JoinHandle` joined).
///
/// ## `TaskOutcome` vs lifecycle events (one truth, two planes)
///
/// `TaskOutcome` is the **authoritative terminal classification** of a run; it is delivered on the guaranteed completion plane (a `oneshot`, immune to bus lag).
/// The [`EventKind`](crate::EventKind) events on the lossy observability bus are the per-attempt narration of the *same* run - they may be dropped under load and
/// a single `EventKind` (notably `ActorExhausted`) is intentionally reused for several terminal outcomes, discriminated only by its `reason` string.
///
/// When you need *the* final result, read the outcome; use events for live progress and metrics.
///
/// | `TaskOutcome`                        | Terminal event(s) on the bus                                          | Notes                                                    |
/// |--------------------------------------|-----------------------------------------------------------------------|----------------------------------------------------------|
/// | [`Completed`](Self::Completed)       | `ActorExhausted` (reason `policy_exhausted_success`)                  | success under `Never`/`OnFailure`                        |
/// | [`Failed`](Self::Failed)             | `ActorExhausted` (reason = failure / `max_retries_exceeded(..)`)      | `reason`/`exit_code` are **byte-identical** to the event |
/// | [`Fatal`](Self::Fatal)               | `ActorDead` (reason = fatal message)                                  | `reason`/`exit_code` byte-identical to the event         |
/// | [`Canceled`](Self::Canceled)         | `TaskCanceled`, or `ActorExhausted` (reason `task_returned_canceled`) | cooperative stop                                         |
/// | [`ForceAborted`](Self::ForceAborted) | `TaskRemoved` (reason `force_terminated_after_grace`)                 | ignored cancellation                                     |
/// | [`Panicked`](Self::Panicked)         | `ActorDead` (reason `actor_panic`)                                    | actor-level panic (not a task-body panic)                |
/// | [`Rejected`](Self::Rejected)         | `ControllerRejected`, or `TaskAddFailed` (reason `already_exists`)     | never ran; controller path only                          |
///
/// # Also
///
/// - [`SupervisorHandle::add_and_watch`](crate::SupervisorHandle::add_and_watch) - obtains a [`TaskWaiter`]
/// - [`SupervisorHandle::submit_and_watch`](crate::SupervisorHandle::submit_and_watch) - controller-path waiter
/// - [`RestartPolicy`](crate::RestartPolicy) / [`BackoffPolicy`](crate::BackoffPolicy) - decide how many attempts happen first
/// - [`EventKind`](crate::EventKind) - per-attempt observability on the event bus
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum TaskOutcome {
    /// Final attempt succeeded and the restart policy did not require another run.
    Completed,

    /// Final attempt failed and no further retries were allowed (non-retryable error, `RestartPolicy::Never`, or `max_retries` exhausted).
    #[non_exhaustive]
    Failed {
        /// Final failure message (byte-identical to the `ActorExhausted` event reason).
        reason: Arc<str>,
        /// Numeric exit code from a process-like runtime; `None` for logical errors.
        exit_code: Option<i32>,
        /// Underlying cause, preserved end-to-end from [`TaskError`](crate::TaskError) (see [`source`](Self::source)).
        source: Option<SharedError>,
    },

    /// Task returned [`TaskError::Fatal`](crate::TaskError::Fatal); no retries were attempted.
    #[non_exhaustive]
    Fatal {
        /// Fatal error message (byte-identical to the `ActorDead` event reason).
        reason: Arc<str>,
        /// Numeric exit code from a process-like runtime; `None` for logical errors.
        exit_code: Option<i32>,
        /// Underlying cause, preserved end-to-end from [`TaskError`](crate::TaskError) (see [`source`](Self::source)).
        source: Option<SharedError>,
    },

    /// Task was canceled:
    /// shutdown, explicit [`remove`](crate::SupervisorHandle::remove)/[`cancel`](crate::SupervisorHandle::cancel), or the task itself returned [`TaskError::Canceled`](crate::TaskError::Canceled).
    Canceled,

    /// Task ignored cooperative cancellation and was force-aborted after the grace period.
    ForceAborted,

    /// The actor itself panicked (a bug guard; panics in the **task body** are caught earlier and surface as [`Failed`](Self::Failed) after retries).
    Panicked,

    /// The submission never ran:
    /// the slot was busy (`DropIfRunning`), the per-slot queue was full, the submission was superseded by a later `Replace`,
    /// it was removed while still queued, the controller was shutting down, or registration was rejected because the task name already exists. **The task body never ran.**
    ///
    /// Observed on the controller path ([`submit_and_watch`](crate::SupervisorHandle::submit_and_watch)).
    Rejected {
        /// Why the submission was refused (e.g. the `ControllerRejected` event reason, or `already_exists` for a duplicate-name registration rejection).
        reason: Arc<str>,
    },
}

impl TaskOutcome {
    /// Returns `true` if the task finished successfully.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, TaskOutcome::Completed)
    }

    /// Returns the underlying error that caused a [`Failed`](Self::Failed)/[`Fatal`](Self::Fatal)
    /// outcome, if a cause was attached (e.g. via [`TaskError::fail_from`](crate::TaskError::fail_from)).
    ///
    /// Enables `downcast_ref` and `anyhow`/`eyre` chaining on the guaranteed completion plane.
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

    /// Stable machine-readable label (for metrics/logging).
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

/// Awaitable handle resolving to the [`TaskOutcome`] of a single task run.
///
/// Created by [`SupervisorHandle::add_and_watch`](crate::SupervisorHandle::add_and_watch).
/// Consumed by [`wait`](Self::wait) (a task terminates exactly once, so the outcome is delivered exactly once).
///
/// ## Example
/// ```rust,no_run
/// # use std::time::Duration;
/// # use taskvisor::prelude::*;
/// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
/// # let handle = sup.serve();
/// let job: TaskRef = TaskFn::arc("job", |_ctx: TaskContext| async { Ok(()) });
/// let (id, waiter) = handle
///     .add_and_watch(TaskSpec::once(job), Duration::from_secs(1))
///     .await?;
///
/// match waiter.wait().await? {
///     TaskOutcome::Completed => println!("{id} done"),
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
    /// Creates a waiter for the given task identity (crate-internal).
    pub(crate) fn new(id: TaskId, rx: oneshot::Receiver<TaskOutcome>) -> Self {
        Self { id, rx }
    }

    /// Runtime identity of the task this waiter observes.
    #[must_use]
    pub fn id(&self) -> TaskId {
        self.id
    }

    /// Waits for the task to fully terminate and returns its final outcome.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::ShuttingDown`] if the runtime dropped the sending half without resolving the outcome (registry stopped before the task was processed).
    /// During a normal shutdown tasks resolve to [`TaskOutcome::Canceled`] / [`TaskOutcome::ForceAborted`] instead.
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
