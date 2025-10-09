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
//! TaskStarting → [task execution] → TaskStopped (success)
//!                                 → TimeoutHit (timeout)
//!                                 → TaskFailed (error)
//!
//! If retry scheduled:
//!   → BackoffScheduled → [sleep] → (next attempt with new seq)
//!
//! On exit:
//!   → ActorExhausted (policy forbids restart)
//!   → ActorDead (fatal error)
//!   → (no event if canceled)
//!```
//!
//! ## Architecture
//! ```text
//! TaskSpec ──► Supervisor ──► TaskActor::run()
//!
//! loop {
//!   ├─► check cancellation (fast-path)
//!   ├─► acquire semaphore (cancellable)
//!   ├─► check cancellation (after acquire)
//!   ├─► attempt += 1
//!   ├─► publish TaskStarting
//!   ├─► run_once() ─► task.spawn()
//!   │       │              ▼
//!   │       │        (one attempt)
//!   │       │              ▼
//!   │       └─► runner publishes:
//!   │             - TaskStopped (success)
//!   │             - TaskFailed (error)
//!   │             - TimeoutHit (timeout)
//!   ├─► apply RestartPolicy
//!   │     ├─► Never       → publish ActorExhausted → break
//!   │     ├─► OnFailure   → if success: publish ActorExhausted → break
//!   │     └─► Always      → continue (on success)
//!   └─► on retryable failure:
//!        ├─► publish BackoffScheduled
//!        └─► sleep(backoff_delay) (cancellable)
//! }
//! ```
//!
//! ## Rules
//! - Attempts run **sequentially** within one actor (never parallel)
//! - Attempt counter **increments on each spawn** (monotonic, never resets)
//! - Events have **monotonic sequence numbers** (ordering guarantees)

use std::{sync::Arc, time::Duration};

use tokio::{select, sync::Semaphore, time};
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
    pub async fn run(self, runtime_token: CancellationToken) -> ActorExitReason {
        let mut prev_delay: Option<Duration> = None;
        let mut attempt: u64 = 0;
        let task_name = self.task.name().to_string();

        loop {
            if runtime_token.is_cancelled() {
                return ActorExitReason::Cancelled;
            }
            let permit = match &self.semaphore {
                Some(sem) => {
                    let fut = sem.clone().acquire_owned();
                    tokio::pin!(fut);
                    select! {
                        res = &mut fut => match res {
                            Ok(p) => Some(p),
                            Err(_closed) => {
                                self.bus.publish(
                                    Event::now(EventKind::ActorExhausted)
                                        .with_task(&task_name)
                                        .with_error("semaphore_closed")
                                );
                                return ActorExitReason::Cancelled;
                            }
                        },
                        _ = runtime_token.cancelled() => return ActorExitReason::Cancelled,
                    }
                }
                None => None,
            };
            if runtime_token.is_cancelled() {
                drop(permit);
                return ActorExitReason::Cancelled;
            }

            attempt += 1;
            self.bus.publish(
                Event::now(EventKind::TaskStarting)
                    .with_task(&task_name)
                    .with_attempt(attempt),
            );
            let res = run_once(
                self.task.as_ref(),
                &runtime_token,
                self.params.timeout,
                attempt,
                &self.bus,
            )
            .await;
            drop(permit);

            match res {
                Ok(()) => {
                    prev_delay = None;

                    match self.params.restart {
                        RestartPolicy::Always => {
                            if let Some(d) = self.params.backoff.success_delay {
                                self.bus.publish(
                                    Event::now(EventKind::BackoffScheduled)
                                        .with_task(&task_name)
                                        .with_attempt(attempt)
                                        .with_delay(d),
                                );
                                let sleep = time::sleep(d);
                                tokio::pin!(sleep);
                                select! {
                                    _ = &mut sleep => {},
                                    _ = runtime_token.cancelled() => {
                                        return ActorExitReason::Cancelled;
                                    }
                                }
                            }
                            continue;
                        }
                        RestartPolicy::OnFailure | RestartPolicy::Never => {
                            self.bus.publish(
                                Event::now(EventKind::ActorExhausted)
                                    .with_task(&task_name)
                                    .with_attempt(attempt)
                                    .with_error("policy_exhausted_success"),
                            );
                            return ActorExitReason::PolicyExhausted;
                        }
                    }
                }
                Err(e) if e.is_fatal() => {
                    self.bus.publish(
                        Event::now(EventKind::ActorDead)
                            .with_task(&task_name)
                            .with_attempt(attempt)
                            .with_error(e.to_string()),
                    );
                    return ActorExitReason::Fatal;
                }
                Err(TaskError::Canceled) => {
                    return ActorExitReason::Cancelled;
                }
                Err(e) => {
                    let policy_allows_retry = matches!(
                        self.params.restart,
                        RestartPolicy::OnFailure | RestartPolicy::Always
                    );
                    let error_is_retryable = e.is_retryable();

                    if !(policy_allows_retry && error_is_retryable) {
                        self.bus.publish(
                            Event::now(EventKind::ActorExhausted)
                                .with_task(&task_name)
                                .with_attempt(attempt)
                                .with_error(e.to_string()),
                        );
                        return ActorExitReason::PolicyExhausted;
                    }

                    let delay = self.params.backoff.next(prev_delay);
                    prev_delay = Some(delay);
                    self.bus.publish(
                        Event::now(EventKind::BackoffScheduled)
                            .with_task(&task_name)
                            .with_delay(delay)
                            .with_attempt(attempt)
                            .with_error(e.to_string()),
                    );

                    let sleep = time::sleep(delay);
                    tokio::pin!(sleep);
                    select! {
                        _ = &mut sleep => {},
                        _ = runtime_token.cancelled() => {
                            return ActorExitReason::Cancelled;
                        }
                    }
                }
            }
        }
    }
}
