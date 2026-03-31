//! # TaskActor: single-task supervisor.
//!
//! Supervises execution of one [`Task`] with policies:
//! - restarts per [`RestartPolicy`],
//! - delays per [`BackoffPolicy`],
//! - optional per-attempt timeout,
//!
//! *Cooperative cancellation via `CancellationToken`.*
//!
//! ## Event flow
//!
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
//!
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
//!   │     ├─► OnFailure   → if success: publish ActorExhausted → break
//!   │     ├─► Never       → publish ActorExhausted → break
//!   │     └─► Always      → continue (on success)
//!   └─► on retryable failure:
//!        ├─► publish BackoffScheduled
//!        └─► sleep(backoff_delay) with runtime_token (cancellable)
//! }
//! ```
//!
//! ## Rules
//!
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
pub(crate) enum ActorExitReason {
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
    Fatal,
}

/// Configuration parameters for a task actor.
#[derive(Clone)]
pub(crate) struct TaskActorParams {
    /// When to restart the task.
    pub(crate) restart: RestartPolicy,
    /// How to compute retry delays.
    pub(crate) backoff: BackoffPolicy,
    /// Optional per-attempt timeout (`None` = no timeout).
    pub(crate) timeout: Option<Duration>,
    /// Maximum retry attempts after failure (`0` = unlimited).
    pub(crate) max_retries: u32,
}

/// Supervises execution of a single [`Task`] with retries, backoff, and event publishing.
///
/// # Also
///
/// - [`run_once`](super::runner::run_once) - executes a single attempt
/// - [`Registry`](super::registry::Registry) - spawns and owns task actors
/// - [`TaskSpec`](crate::TaskSpec) - user-facing configuration that maps to [`TaskActorParams`]
pub(crate) struct TaskActor {
    /// Task to execute.
    task: Arc<dyn Task>,
    /// Parameters for supervised task executions.
    params: TaskActorParams,
    /// Internal event bus (used to publish lifecycle events).
    bus: Bus,
    /// Optional global tasks concurrency limiter.
    semaphore: Option<Arc<Semaphore>>,
}

impl TaskActor {
    /// Creates a new task actor.
    pub(crate) fn new(
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
    /// We run each attempt under a **child** cancellation token so that cancelling the current attempt
    /// (including backoff sleep) results in a clean exit without restarting, even with `RestartPolicy::Always`.
    pub(crate) async fn run(self, runtime_token: CancellationToken) -> ActorExitReason {
        let task_name: Arc<str> = Arc::from(self.task.name());
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
                    let retries_exhausted =
                        self.params.max_retries > 0 && backoff_attempt >= self.params.max_retries;

                    if !(policy_allows_retry && error_is_retryable) || retries_exhausted {
                        let reason = if retries_exhausted {
                            format!(
                                "max_retries_exceeded({}/{}): {}",
                                backoff_attempt, self.params.max_retries, e
                            )
                        } else {
                            e.to_string()
                        };
                        self.bus.publish(
                            Event::new(EventKind::ActorExhausted)
                                .with_task(task_name.clone())
                                .with_attempt(attempt)
                                .with_reason(reason),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU32, Ordering};

    type BoxFut = Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'static>>;

    fn fast_backoff() -> BackoffPolicy {
        BackoffPolicy {
            first: Duration::from_millis(1),
            max: Duration::from_millis(1),
            factor: 1.0,
            jitter: crate::JitterPolicy::None,
        }
    }

    fn params(restart: RestartPolicy, max_retries: u32) -> TaskActorParams {
        TaskActorParams {
            restart,
            backoff: fast_backoff(),
            timeout: None,
            max_retries,
        }
    }

    fn actor(task: Arc<dyn Task>, restart: RestartPolicy, max_retries: u32) -> TaskActor {
        TaskActor::new(
            Bus::new(16),
            Arc::clone(&task),
            params(restart, max_retries),
            None,
        )
    }

    struct OkTask;
    impl Task for OkTask {
        fn name(&self) -> &str {
            "ok"
        }
        fn spawn(&self, _ctx: CancellationToken) -> BoxFut {
            Box::pin(async { Ok(()) })
        }
    }

    struct FailTask;
    impl Task for FailTask {
        fn name(&self) -> &str {
            "fail"
        }
        fn spawn(&self, _ctx: CancellationToken) -> BoxFut {
            Box::pin(async {
                Err(TaskError::Fail {
                    reason: "boom".into(),
                })
            })
        }
    }

    struct FatalTask;
    impl Task for FatalTask {
        fn name(&self) -> &str {
            "fatal"
        }
        fn spawn(&self, _ctx: CancellationToken) -> BoxFut {
            Box::pin(async {
                Err(TaskError::Fatal {
                    reason: "fatal".into(),
                })
            })
        }
    }

    struct CountedTask {
        remaining: AtomicU32,
    }
    impl CountedTask {
        fn new(fail_count: u32) -> Self {
            Self {
                remaining: AtomicU32::new(fail_count),
            }
        }
    }
    impl Task for CountedTask {
        fn name(&self) -> &str {
            "counted"
        }
        fn spawn(&self, _ctx: CancellationToken) -> BoxFut {
            let prev = self.remaining.fetch_sub(1, Ordering::SeqCst);
            if prev > 0 {
                Box::pin(async {
                    Err(TaskError::Fail {
                        reason: "transient".into(),
                    })
                })
            } else {
                Box::pin(async { Ok(()) })
            }
        }
    }

    #[tokio::test]
    async fn never_ok_returns_policy_exhausted() {
        let a = actor(Arc::new(OkTask), RestartPolicy::Never, 0);
        let reason = a.run(CancellationToken::new()).await;
        assert_eq!(reason, ActorExitReason::PolicyExhausted);
    }

    #[tokio::test]
    async fn on_failure_ok_returns_policy_exhausted() {
        let a = actor(Arc::new(OkTask), RestartPolicy::OnFailure, 0);
        let reason = a.run(CancellationToken::new()).await;
        assert_eq!(reason, ActorExitReason::PolicyExhausted);
    }

    #[tokio::test]
    async fn fatal_error_returns_fatal() {
        let a = actor(Arc::new(FatalTask), RestartPolicy::OnFailure, 0);
        let reason = a.run(CancellationToken::new()).await;
        assert_eq!(reason, ActorExitReason::Fatal);
    }

    #[tokio::test]
    async fn max_retries_exhausted_returns_policy_exhausted() {
        let a = actor(Arc::new(FailTask), RestartPolicy::OnFailure, 3);
        let reason = a.run(CancellationToken::new()).await;
        assert_eq!(reason, ActorExitReason::PolicyExhausted);
    }

    #[tokio::test]
    async fn cancellation_returns_cancelled() {
        let token = CancellationToken::new();
        token.cancel();
        let a = actor(
            Arc::new(OkTask),
            RestartPolicy::Always { interval: None },
            0,
        );
        let reason = a.run(token).await;
        assert_eq!(reason, ActorExitReason::Cancelled);
    }

    #[tokio::test]
    async fn on_failure_retries_then_succeeds() {
        let task = Arc::new(CountedTask::new(2));
        let a = actor(task, RestartPolicy::OnFailure, 0);
        let reason = a.run(CancellationToken::new()).await;
        assert_eq!(reason, ActorExitReason::PolicyExhausted);
    }
}
