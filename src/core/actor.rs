//! # TaskActor: single-task supervisor.
//!
//! Supervises execution of one [`Task`] with policies:
//! - restarts per [`RestartPolicy`],
//! - delays per [`BackoffPolicy`],
//! - optional per-attempt timeout,
//! - cooperative cancellation via [`CancellationToken`].
//!
//! ## Event flow
//! For each attempt, the actor publishes:
//! ```text
//! TaskStarting → [task execution] → TaskStopped (success/cancel)
//!                                 → TimeoutHit (timeout)
//!                                 → TaskFailed (error)
//!
//! If retry scheduled:
//!   → BackoffScheduled → [sleep] → (next attempt with new seq)
//!
//! On exit:
//!   → ActorExhausted (policy forbids restart)
//!   → ActorDead (fatal error)
//!   → (no terminal event if cancelled - runner already published TaskStopped)
//!```
//!
//! ## Architecture
//! ```text
//! TaskSpec ──► Supervisor ──► TaskActor::run()
//!
//! loop {
//!   ├─► check cancellation (fast-path with runtime_token)
//!   ├─► acquire semaphore (cancellable with runtime_token)
//!   ├─► check cancellation (after acquire)
//!   ├─► create child_token (isolated per attempt)
//!   ├─► attempt += 1
//!   ├─► publish TaskStarting
//!   ├─► run_once(child_token) ─► task.spawn()
//!   │       │              ▼
//!   │       │        (one attempt)
//!   │       │              ▼
//!   │       └─► runner publishes:
//!   │             - TaskStopped (success/cancel)
//!   │             - TaskFailed (error)
//!   │             - TimeoutHit (timeout)
//!   ├─► drop permit (release semaphore)
//!   ├─► child_token goes out of scope
//!   ├─► apply RestartPolicy
//!   │     ├─► Never       → publish ActorExhausted → break
//!   │     ├─► OnFailure   → if success: publish ActorExhausted → break
//!   │     └─► Always      → continue (on success)
//!   └─► on retryable failure:
//!        ├─► publish BackoffScheduled
//!        └─► sleep(backoff_delay) with runtime_token (cancellable)
//! }
//! ```
//!
//! ## Rules
//! - Attempts run **sequentially** within one actor (never parallel)
//! - Attempt counter **increments on each spawn** (monotonic, never resets)
//! - Events have **monotonic sequence numbers** (ordering guarantees)
//! - Each attempt gets its own **child_token** for isolation

use std::{sync::Arc, time::Duration};

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::{
    TaskError,
    core::runner::run_once,
    events::{Bus, Event, EventKind},
    policies::{BackoffPolicy, RestartPolicy},
    tasks::Task,
};

/// Reason why a task actor exited.
///
/// Used to determine what event to publish and whether to clean up the task from registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorExitReason {
    /// Actor exhausted its restart policy and will not restart.
    ///
    /// Occurs when:
    /// - `RestartPolicy::Never` and task completed (success or failure)
    /// - `RestartPolicy::OnFailure` and task completed successfully
    PolicyExhausted,

    /// Actor was canceled due to shut down signal or explicit removal.
    ///
    /// The task detected `CancellationToken::is_cancelled()` and exited gracefully.
    Cancelled,

    /// Actor died due to a fatal error that should not be retried.
    ///
    /// Occurs when:
    /// - Task returned `TaskError::Fatal`
    /// - (Future) Max retries exceeded
    Fatal,
}

/// Configuration parameters for a task actor.
#[derive(Clone)]
pub struct TaskActorParams {
    /// When to restart the task.
    pub restart: RestartPolicy,
    /// How to compute retry delays.
    pub backoff: BackoffPolicy,
    /// Optional per-attempt timeout (`None` = no timeout).
    pub timeout: Option<Duration>,
}

/// Supervises execution of a single [`Task`] with retries, backoff, and event publishing.
pub struct TaskActor {
    /// Task to execute.
    pub task: Arc<dyn Task>,
    /// Parameters for supervised task executions.
    pub params: TaskActorParams,
    /// Internal event bus (used to publish lifecycle events).
    pub bus: Bus,
    /// Optional global tasks concurrency limiter.
    pub semaphore: Option<Arc<Semaphore>>,
}

impl TaskActor {
    /// Creates a new task actor.
    pub fn new(
        bus: Bus,
        task: Arc<dyn Task>,
        params: TaskActorParams,
        semaphore: Option<Arc<Semaphore>>,
    ) -> Self {
        Self {
            task,
            params,
            bus,
            semaphore,
        }
    }

    /// Runs the actor until completion, restart exhaustion, or cancellation.
    ///
    /// We run each attempt under a **child** cancellation token so that cancelling the
    /// current attempt (including backoff sleep) results in a clean exit without
    /// restarting, even with `RestartPolicy::Always`.
    pub async fn run(self, runtime_token: CancellationToken) -> ActorExitReason {
        let task_name: Arc<str> = Arc::from(self.task.name().to_owned());
        let mut attempt: u32 = 0;
        let mut backoff_attempt: u32 = 0;

        loop {
            if runtime_token.is_cancelled() {
                return ActorExitReason::Cancelled;
            }
            let permit = match &self.semaphore {
                Some(sem) => {
                    let fut = sem.clone().acquire_owned();
                    tokio::pin!(fut);

                    tokio::select! {
                        res = &mut fut => match res {
                            Ok(p) => Some(p),
                            Err(_closed) => {
                                self.bus.publish(
                                    Event::new(EventKind::ActorExhausted)
                                        .with_task(task_name.clone())
                                        .with_attempt(attempt)
                                        .with_reason("semaphore_closed")
                                );
                                return ActorExitReason::Cancelled;
                            }
                        },
                        _ = runtime_token.cancelled() => {
                            return ActorExitReason::Cancelled;
                        }
                    }
                }
                None => None,
            };
            if runtime_token.is_cancelled() {
                drop(permit);
                return ActorExitReason::Cancelled;
            }

            let child = runtime_token.child_token();
            attempt += 1;

            self.bus.publish(
                Event::new(EventKind::TaskStarting)
                    .with_task(task_name.clone())
                    .with_attempt(attempt),
            );
            let res = run_once(
                self.task.as_ref(),
                &child,
                self.params.timeout,
                attempt,
                &self.bus,
            )
            .await;

            drop(permit);
            match res {
                Ok(()) => {
                    backoff_attempt = 0;

                    match self.params.restart {
                        RestartPolicy::Always { interval } => {
                            if let Some(d) = interval {
                                self.bus.publish(
                                    Event::new(EventKind::BackoffScheduled)
                                        .with_backoff_success()
                                        .with_task(task_name.clone())
                                        .with_attempt(attempt)
                                        .with_delay(d),
                                );
                                if !Self::sleep_cancellable(d, &runtime_token).await {
                                    return ActorExitReason::Cancelled;
                                }
                            }
                            continue;
                        }
                        RestartPolicy::OnFailure | RestartPolicy::Never => {
                            self.bus.publish(
                                Event::new(EventKind::ActorExhausted)
                                    .with_task(task_name.clone())
                                    .with_attempt(attempt)
                                    .with_reason("policy_exhausted_success"),
                            );
                            return ActorExitReason::PolicyExhausted;
                        }
                    }
                }
                Err(e) if e.is_fatal() => {
                    self.bus.publish(
                        Event::new(EventKind::ActorDead)
                            .with_task(task_name.clone())
                            .with_attempt(attempt)
                            .with_reason(e.to_string()),
                    );
                    return ActorExitReason::Fatal;
                }
                Err(TaskError::Canceled) => {
                    return ActorExitReason::Cancelled;
                }
                Err(e) => {
                    let policy_allows_retry = matches!(
                        self.params.restart,
                        RestartPolicy::OnFailure | RestartPolicy::Always { .. }
                    );
                    let error_is_retryable = e.is_retryable();

                    if !(policy_allows_retry && error_is_retryable) {
                        self.bus.publish(
                            Event::new(EventKind::ActorExhausted)
                                .with_task(task_name.clone())
                                .with_attempt(attempt)
                                .with_reason(e.to_string()),
                        );
                        return ActorExitReason::PolicyExhausted;
                    }

                    let delay = self.params.backoff.next(backoff_attempt);
                    backoff_attempt += 1;

                    self.bus.publish(
                        Event::new(EventKind::BackoffScheduled)
                            .with_backoff_failure()
                            .with_task(task_name.clone())
                            .with_delay(delay)
                            .with_attempt(attempt)
                            .with_reason(e.to_string()),
                    );
                    if !Self::sleep_cancellable(delay, &runtime_token).await {
                        return ActorExitReason::Cancelled;
                    }
                }
            }
        }
    }

    /// Sleep helper that can be interrupted by a cancellation token.
    ///
    /// Returns `true` if sleep completed, `false` if cancelled.
    #[inline]
    async fn sleep_cancellable(duration: Duration, token: &CancellationToken) -> bool {
        let sleep = tokio::time::sleep(duration);
        tokio::pin!(sleep);

        tokio::select! {
            _ = &mut sleep => true,
            _ = token.cancelled() => false,
        }
    }
}
