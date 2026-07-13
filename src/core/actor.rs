//! # Restart loop for one task
//!
//! [`TaskActor`] runs attempts in order and applies restart, backoff, timeout, retry-limit, and cancellation rules.
//! One actor belongs to one registered [`TaskId`].
//!
//! ## Flow
//!
//! ```text
//! wait for permit -> TaskStarting -> run one attempt -> release permit
//!                                      |
//!                  +-------------------+-------------------+
//!                  |                   |                   |
//!               success         retryable failure    fatal/canceled
//!                  |                   |                   |
//!           stop or repeat       backoff or stop           stop
//! ```
//!
//! [`run_once`] handles one attempt, including timeout, panic capture, and attempt events.
//! The actor then decides whether to stop, wait, or start another attempt.
//!
//! ## Events and Final State
//!
//! `TaskStopped`, `TaskCanceled`, and `TaskFailed` describe one attempt.
//! `BackoffScheduled`, `ActorExhausted`, and `ActorDead` describe the actor's decision.
//!
//! A `TaskFailed` event is not a final outcome; another attempt may follow it.
//!
//! ## Rules
//!
//! - Attempts are sequential inside one actor.
//! - Attempt numbers start at 1.
//! - `max_retries` counts retries in one failure streak. A success resets it.
//! - A concurrency permit is held only while an attempt runs, not during delays.
//! - Cancellation can stop permit waits and retry delays.
//! - Instant successful repeats have a small delay to prevent a hot loop.
//! - User-task panics become retryable `TaskError::Fail` values.

use std::{
    num::NonZeroU32,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::{
    TaskError,
    core::runner::run_once,
    error::SharedError,
    events::{Bus, Event, EventKind},
    identity::TaskId,
    policies::{BackoffPolicy, RestartPolicy},
    reasons,
    tasks::Task,
};

/// Small delay used to prevent hot restart loops.
///
/// This applies to `RestartPolicy::Always` when a task finishes very quickly.
/// Without it, a task that returns `Ok(())` at once could restart as fast as the
/// scheduler allows and flood the event bus.
///
/// The delay is only added when the task ran for less than this value.
/// If the task already spent enough time doing work, no extra delay is added.
const IMMEDIATE_RESTART_FLOOR: Duration = Duration::from_millis(1);

/// Returns the delay after a successful `Always` attempt.
///
/// The small restart guard can make a very short configured delay longer.
fn floored_interval(interval: Duration, elapsed: Duration) -> Duration {
    interval.max(IMMEDIATE_RESTART_FLOOR.saturating_sub(elapsed))
}

/// Final reason returned by [`TaskActor::run`].
///
/// After joining the actor, the registry maps this value to the matching
/// [`TaskOutcome`](crate::TaskOutcome).
#[derive(Debug, Clone)]
pub(crate) enum ActorExitReason {
    /// Final attempt succeeded and the restart policy stopped the actor.
    ///
    /// Occurs when:
    /// - `RestartPolicy::Never` and the task completed successfully
    /// - `RestartPolicy::OnFailure` and the task completed successfully
    Completed,

    /// Final attempt failed and the actor stopped without a fatal error.
    ///
    /// Occurs when:
    /// - `RestartPolicy::Never` does not allow a retry
    /// - the error is not retryable
    /// - the retry budget (`max_retries`) is used up
    Exhausted {
        /// Final failure message. Same text as the `ActorExhausted` event reason.
        reason: Arc<str>,
        /// Numeric exit code from a process-like task, if any.
        exit_code: Option<i32>,
        /// Original error source from the final [`TaskError`], if any.
        source: Option<SharedError>,
    },

    /// Actor stopped because of runtime shutdown, explicit removal, or `TaskError::Canceled`.
    ///
    /// This maps to [`TaskOutcome::Canceled`](crate::TaskOutcome).
    /// Depending on where cancellation happened, there may be no actor-level terminal event.
    Canceled,

    /// Actor stopped because the task returned a fatal error.
    ///
    /// Fatal errors are not retried.
    Fatal {
        /// Fatal error message. Same text as the `ActorDead` event reason.
        reason: Arc<str>,
        /// Numeric exit code from a process-like task, if any.
        exit_code: Option<i32>,
        /// Original error source from the fatal [`TaskError`], if any.
        source: Option<SharedError>,
    },
}

/// Runtime parameters used by one task actor.
#[derive(Clone)]
pub(crate) struct TaskActorParams {
    /// Policy that decides whether another attempt is allowed.
    pub(crate) restart: RestartPolicy,
    /// Delay policy for retryable failures.
    pub(crate) backoff: BackoffPolicy,
    /// Optional timeout for one attempt (`None` = no timeout).
    pub(crate) timeout: Option<Duration>,
    /// Maximum retries after the first failed attempt (`None` = unlimited).
    pub(crate) max_retries: Option<NonZeroU32>,
}

/// Internal supervisor for one registered task.
///
/// The registry spawns one actor per accepted task.
/// The actor runs attempts sequentially and returns one [`ActorExitReason`] when the retry loop ends.
pub(crate) struct TaskActor {
    /// Runtime identity stamped on lifecycle events for this task run.
    id: TaskId,
    /// Task label.
    name: Arc<str>,
    /// Task to execute.
    task: Arc<dyn Task>,
    /// Restart, backoff, timeout, and retry settings.
    params: TaskActorParams,
    /// Internal event bus used for lifecycle events.
    bus: Bus,
    /// Optional global limiter for concurrently running attempts.
    ///
    /// Held only while `run_once` is executing. Retry/backoff sleeps do not hold it.
    semaphore: Option<Arc<Semaphore>>,
}

impl TaskActor {
    /// Creates an actor for one accepted task registration.
    pub(crate) fn new(
        bus: Bus,
        name: Arc<str>,
        task: Arc<dyn Task>,
        params: TaskActorParams,
        semaphore: Option<Arc<Semaphore>>,
        id: TaskId,
    ) -> Self {
        Self {
            id,
            name,
            task,
            params,
            bus,
            semaphore,
        }
    }

    /// Runs the actor until completion, retry exhaustion, fatal failure, or cancellation.
    ///
    /// `run_once` derives a child token for the current attempt.
    /// The runtime token lets shutdown interrupt permit waits and retry delays.
    pub(crate) async fn run(self, runtime_token: CancellationToken) -> ActorExitReason {
        let task_name: Arc<str> = self.name.clone();
        let id = self.id;
        let mut attempt: u32 = 0;
        let mut backoff_attempt: u32 = 0;

        loop {
            if runtime_token.is_cancelled() {
                return ActorExitReason::Canceled;
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
                                        .with_id(id)
                                        .with_attempt(attempt)
                                        .with_reason("semaphore_closed"),
                                );
                                return ActorExitReason::Canceled;
                            }
                        },
                        _ = runtime_token.cancelled() => {
                            return ActorExitReason::Canceled;
                        }
                    }
                }
                None => None,
            };
            if runtime_token.is_cancelled() {
                drop(permit);
                return ActorExitReason::Canceled;
            }

            attempt = attempt.saturating_add(1);

            self.bus.publish(
                Event::new(EventKind::TaskStarting)
                    .with_task(task_name.clone())
                    .with_id(id)
                    .with_attempt(attempt),
            );
            let attempt_start = Instant::now();
            let res = run_once(
                self.task.as_ref(),
                &runtime_token,
                self.params.timeout,
                attempt,
                id,
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
                                let delay = floored_interval(d, attempt_start.elapsed());
                                self.bus.publish(
                                    Event::new(EventKind::BackoffScheduled)
                                        .with_backoff_success()
                                        .with_task(task_name.clone())
                                        .with_id(id)
                                        .with_attempt(attempt)
                                        .with_delay(delay),
                                );
                                if !Self::sleep_cancellable(delay, &runtime_token).await {
                                    return ActorExitReason::Canceled;
                                }
                            } else {
                                let elapsed = attempt_start.elapsed();
                                if elapsed < IMMEDIATE_RESTART_FLOOR {
                                    if !Self::sleep_cancellable(
                                        IMMEDIATE_RESTART_FLOOR - elapsed,
                                        &runtime_token,
                                    )
                                    .await
                                    {
                                        return ActorExitReason::Canceled;
                                    }
                                } else {
                                    tokio::task::yield_now().await;
                                }
                            }
                            continue;
                        }
                        RestartPolicy::OnFailure | RestartPolicy::Never => {
                            if runtime_token.is_cancelled() {
                                return ActorExitReason::Canceled;
                            }
                            self.bus.publish(
                                Event::new(EventKind::ActorExhausted)
                                    .with_task(task_name.clone())
                                    .with_id(id)
                                    .with_attempt(attempt)
                                    .with_reason(reasons::POLICY_EXHAUSTED_SUCCESS),
                            );
                            return ActorExitReason::Completed;
                        }
                    }
                }
                Err(e) if e.is_fatal() => {
                    let reason: Arc<str> = Arc::from(e.to_string());
                    let exit_code = e.exit_code();
                    let source: Option<SharedError> = e.into_source().map(Arc::from);

                    let mut ev = Event::new(EventKind::ActorDead)
                        .with_task(task_name.clone())
                        .with_id(id)
                        .with_attempt(attempt)
                        .with_reason(Arc::clone(&reason));
                    if let Some(code) = exit_code {
                        ev = ev.with_exit_code(code);
                    }
                    self.bus.publish(ev);
                    return ActorExitReason::Fatal {
                        reason,
                        exit_code,
                        source,
                    };
                }
                Err(TaskError::Canceled) => {
                    if runtime_token.is_cancelled() {
                        return ActorExitReason::Canceled;
                    }
                    self.bus.publish(
                        Event::new(EventKind::ActorExhausted)
                            .with_task(task_name.clone())
                            .with_id(id)
                            .with_attempt(attempt)
                            .with_reason(reasons::TASK_RETURNED_CANCELED),
                    );
                    return ActorExitReason::Canceled;
                }
                Err(e) => {
                    let policy_allows_retry = matches!(
                        self.params.restart,
                        RestartPolicy::OnFailure | RestartPolicy::Always { .. }
                    );
                    let error_is_retryable = e.is_retryable();
                    let retries_exhausted = self
                        .params
                        .max_retries
                        .is_some_and(|max| backoff_attempt >= max.get());

                    if !(policy_allows_retry && error_is_retryable) || retries_exhausted {
                        let reason: Arc<str> = if let Some(limit) =
                            self.params.max_retries.filter(|_| retries_exhausted)
                        {
                            Arc::from(format!(
                                "{}({}/{}): {}",
                                reasons::MAX_RETRIES_EXCEEDED,
                                backoff_attempt,
                                limit.get(),
                                e
                            ))
                        } else {
                            Arc::from(e.to_string())
                        };
                        let exit_code = e.exit_code();
                        let source: Option<SharedError> = e.into_source().map(Arc::from);

                        let mut ev = Event::new(EventKind::ActorExhausted)
                            .with_task(task_name.clone())
                            .with_id(id)
                            .with_attempt(attempt)
                            .with_reason(Arc::clone(&reason));
                        if let Some(code) = exit_code {
                            ev = ev.with_exit_code(code);
                        }
                        self.bus.publish(ev);
                        return ActorExitReason::Exhausted {
                            reason,
                            exit_code,
                            source,
                        };
                    }

                    let delay = self.params.backoff.delay_for_retry(backoff_attempt);
                    backoff_attempt = backoff_attempt.saturating_add(1);

                    self.bus.publish(
                        Event::new(EventKind::BackoffScheduled)
                            .with_backoff_failure()
                            .with_task(task_name.clone())
                            .with_id(id)
                            .with_delay(delay)
                            .with_attempt(attempt)
                            .with_reason(e.to_string()),
                    );
                    if !Self::sleep_cancellable(delay, &runtime_token).await {
                        return ActorExitReason::Canceled;
                    }
                }
            }
        }
    }

    /// Sleeps until `duration` elapses or `token` is cancelled.
    ///
    /// Returns `true` if the sleep finished, or `false` if it was cancelled.
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
    use crate::TaskContext;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU32, Ordering};

    type BoxFut = Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'static>>;

    fn fast_backoff() -> BackoffPolicy {
        BackoffPolicy::new(
            Duration::from_millis(1),
            Duration::from_millis(1),
            1.0,
            crate::JitterPolicy::None,
        )
        .expect("valid backoff")
    }

    fn params(restart: RestartPolicy, max_retries: u32) -> TaskActorParams {
        TaskActorParams {
            restart,
            backoff: fast_backoff(),
            timeout: None,
            max_retries: NonZeroU32::new(max_retries),
        }
    }

    fn actor(task: Arc<dyn Task>, restart: RestartPolicy, max_retries: u32) -> TaskActor {
        let name: Arc<str> = Arc::from(task.name());
        TaskActor::new(
            Bus::new(16),
            name,
            Arc::clone(&task),
            params(restart, max_retries),
            None,
            TaskId::next(),
        )
    }

    struct OkTask;
    impl Task for OkTask {
        fn name(&self) -> &str {
            "ok"
        }
        fn spawn(&self, _ctx: TaskContext) -> BoxFut {
            Box::pin(async { Ok(()) })
        }
    }

    struct FailTask;
    impl Task for FailTask {
        fn name(&self) -> &str {
            "fail"
        }
        fn spawn(&self, _ctx: TaskContext) -> BoxFut {
            Box::pin(async { Err(TaskError::fail("boom")) })
        }
    }

    struct FatalTask;
    impl Task for FatalTask {
        fn name(&self) -> &str {
            "fatal"
        }
        fn spawn(&self, _ctx: TaskContext) -> BoxFut {
            Box::pin(async { Err(TaskError::fatal("fatal")) })
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
        fn spawn(&self, _ctx: TaskContext) -> BoxFut {
            let prev = self.remaining.fetch_sub(1, Ordering::SeqCst);
            if prev > 0 {
                Box::pin(async { Err(TaskError::fail("transient")) })
            } else {
                Box::pin(async { Ok(()) })
            }
        }
    }

    #[tokio::test]
    async fn ok_task_returns_completed_under_non_restarting_policies() {
        for restart in [RestartPolicy::Never, RestartPolicy::OnFailure] {
            let a = actor(Arc::new(OkTask), restart, 0);
            let reason = a.run(CancellationToken::new()).await;
            assert!(
                matches!(reason, ActorExitReason::Completed),
                "{restart:?} + Ok task must exit Completed, got {reason:?}"
            );
        }
    }

    #[tokio::test]
    async fn fatal_error_returns_fatal_with_reason() {
        let a = actor(Arc::new(FatalTask), RestartPolicy::OnFailure, 0);
        let reason = a.run(CancellationToken::new()).await;
        match reason {
            ActorExitReason::Fatal {
                reason, exit_code, ..
            } => {
                assert!(
                    reason.contains("fatal"),
                    "reason must carry the error: {reason}"
                );
                assert_eq!(exit_code, None);
            }
            other => panic!("expected Fatal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn max_retries_exhausted_returns_exhausted_with_reason() {
        let a = actor(Arc::new(FailTask), RestartPolicy::OnFailure, 3);
        let reason = a.run(CancellationToken::new()).await;
        match reason {
            ActorExitReason::Exhausted { reason, .. } => {
                assert!(
                    reason.contains("max_retries_exceeded"),
                    "reason must mention exhausted budget: {reason}"
                );
            }
            other => panic!("expected Exhausted, got {other:?}"),
        }
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
        assert!(matches!(reason, ActorExitReason::Canceled));
    }

    #[tokio::test]
    async fn on_failure_retries_then_succeeds() {
        let task = Arc::new(CountedTask::new(2));
        let a = actor(task, RestartPolicy::OnFailure, 0);
        let reason = a.run(CancellationToken::new()).await;
        assert!(matches!(reason, ActorExitReason::Completed));
    }

    #[tokio::test(start_paused = true)]
    async fn always_none_instant_ok_is_rate_limited() {
        use std::sync::atomic::{AtomicU32, Ordering};

        struct Counting(Arc<AtomicU32>);
        impl Task for Counting {
            fn name(&self) -> &str {
                "spin"
            }
            fn spawn(&self, _ctx: TaskContext) -> BoxFut {
                self.0.fetch_add(1, Ordering::Relaxed);
                Box::pin(async { Ok(()) })
            }
        }

        let counter = Arc::new(AtomicU32::new(0));
        let task = Arc::new(Counting(Arc::clone(&counter)));
        let a = actor(task, RestartPolicy::Always { interval: None }, 0);

        let token = CancellationToken::new();
        let child = token.clone();
        let handle = tokio::spawn(async move { a.run(child).await });

        tokio::time::sleep(Duration::from_millis(25)).await;
        token.cancel();
        let _ = handle.await;
        let n = counter.load(Ordering::Relaxed);
        assert!(
            (1..=200).contains(&n),
            "Always {{ interval: None }} with an instant-Ok task must be floored, got {n} restarts in 25ms"
        );
    }

    #[test]
    fn floored_interval_floors_only_the_idle_portion() {
        let floor = IMMEDIATE_RESTART_FLOOR;

        assert_eq!(floored_interval(Duration::ZERO, Duration::ZERO), floor);
        assert_eq!(floored_interval(floor / 2, Duration::ZERO), floor);
        assert_eq!(
            floored_interval(Duration::ZERO, floor * 2),
            Duration::ZERO,
            "a slow attempt must not be additionally delayed"
        );
        assert_eq!(floored_interval(floor * 10, Duration::ZERO), floor * 10);
        assert_eq!(floored_interval(floor * 10, floor * 3), floor * 10);
    }
}
