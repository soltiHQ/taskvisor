//! Runs the restart loop for one registered task.
//!
//! Registry admission creates one [`TaskActor`] for a [`TaskId`]. The actor runs attempts sequentially,
//! applies restart and backoff policy, and returns one [`ActorExitReason`] to registry cleanup.
//!
//! ```text
//! registry admission ──► TaskActor
//!                            ▼
//!                   wait for attempt permit
//!                            ▼
//!                        run_once
//!                            ├── success ──► stop or repeat
//!                            ├── retryable failure ──► backoff or stop
//!                            └── fatal or canceled ──► stop
//! ```
//!
//! [`run_once`] owns timeout, panic capture, and terminal events for one attempt. The actor owns
//! the retry counter and delays between attempts. A concurrency permit and activity flag remain
//! held until the physical attempt exits. A success resets the failure retry counter.

use std::{
    num::NonZeroU32,
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::{
    TaskError,
    core::runner::{AttemptFailure, AttemptRun, dispose_panic_payload, run_once},
    error::SharedError,
    events::{Bus, Event, EventKind},
    identity::TaskId,
    policies::{BackoffPolicy, RestartPolicy},
    tasks::Task,
};

/// Minimum interval between immediate successful `Always` attempts.
const IMMEDIATE_RESTART_FLOOR: Duration = Duration::from_millis(1);

/// Returns the delay after a successful `Always` attempt.
///
/// The small restart guard can make a very short configured delay longer.
fn floored_interval(interval: Duration, elapsed: Duration) -> Duration {
    interval.max(IMMEDIATE_RESTART_FLOOR.saturating_sub(elapsed))
}

/// Final actor result passed to registry outcome classification.
#[derive(Debug)]
pub(crate) enum ActorExitReason {
    /// A successful attempt stopped under its restart policy.
    Completed,

    /// A non-fatal failure stopped under policy or retry limits.
    Exhausted {
        /// Diagnostic final failure message.
        reason: Arc<str>,
        /// Numeric exit code from a process-like task, if any.
        exit_code: Option<i32>,
        /// Original error source from the final [`TaskError`], if any.
        source: Option<SharedError>,
    },

    /// Cancellation stopped the current attempt or prevented another from beginning.
    Canceled,

    /// User-value cleanup panicked inside the physical actor boundary.
    Panicked {
        /// A nested panic payload destructor had to be retained permanently.
        cleanup_poisoned: bool,
    },

    /// The task returned a fatal error, which is never retried.
    Fatal {
        /// Diagnostic fatal error message.
        reason: Arc<str>,
        /// Numeric exit code from a process-like task, if any.
        exit_code: Option<i32>,
        /// Original error source from the fatal [`TaskError`], if any.
        source: Option<SharedError>,
    },
}

/// Result classified while the actor still owns its attempt permit and activity guard.
enum AttemptDecision {
    /// The actor must return this attempt result.
    Finished(Result<(), AttemptFailure>),
    /// The actor may wait and start another attempt.
    Retry { reason: Arc<str> },
    /// User-value cleanup panicked before guards could be released normally.
    CleanupPanicked,
}

/// Applies restart and retry rules to one attempt result.
fn classify_attempt(
    result: Result<(), AttemptFailure>,
    restart: RestartPolicy,
    max_retries: Option<NonZeroU32>,
    backoff_attempt: u32,
    cleanup_poisoned: &AtomicBool,
) -> AttemptDecision {
    let Err(failure) = result else {
        return AttemptDecision::Finished(Ok(()));
    };
    if failure.cleanup_panicked {
        let AttemptFailure { error, .. } = failure;
        if let Err(payload) = std::panic::catch_unwind(AssertUnwindSafe(|| drop(error))) {
            dispose_panic_payload(payload, cleanup_poisoned);
        }
        return AttemptDecision::CleanupPanicked;
    }
    let policy_allows_retry = matches!(
        restart,
        RestartPolicy::OnFailure | RestartPolicy::Always { .. }
    );
    let retries_exhausted = max_retries.is_some_and(|max| backoff_attempt >= max.get());
    if !(policy_allows_retry && failure.error.is_retryable()) || retries_exhausted {
        return AttemptDecision::Finished(Err(failure));
    }

    let AttemptFailure { error, reason, .. } = failure;
    match std::panic::catch_unwind(AssertUnwindSafe(|| drop(error))) {
        Ok(()) => AttemptDecision::Retry { reason },
        Err(payload) => {
            dispose_panic_payload(payload, cleanup_poisoned);
            AttemptDecision::CleanupPanicked
        }
    }
}

/// Marks the exact physical lifetime of one attempt, including the interval
/// between Tokio abort being requested and a blocked poll actually returning.
struct AttemptActivity(Arc<AtomicBool>);

impl AttemptActivity {
    /// Marks an attempt active until the returned guard is dropped.
    fn begin(activity: Arc<AtomicBool>) -> Self {
        activity.store(true, Ordering::Release);
        Self(activity)
    }
}

impl Drop for AttemptActivity {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
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

/// Registry-owned execution resources shared with one actor.
pub(crate) struct TaskActorResources {
    /// Optional supervisor-wide attempt concurrency limit.
    pub(crate) semaphore: Option<Arc<Semaphore>>,
    /// Registry activity flag for this task entry.
    pub(crate) activity: Arc<AtomicBool>,
    /// Records an unrecoverable nested cleanup panic.
    pub(crate) cleanup_poisoned: Arc<AtomicBool>,
}

/// Owns the sequential attempt loop for one registered task.
pub(crate) struct TaskActor {
    /// Runtime identity stamped on lifecycle events for this task run.
    id: TaskId,
    /// Task name.
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
    /// Authoritative per-entry attempt activity bit.
    activity: Arc<AtomicBool>,
    /// Records a nested cleanup panic whose payload had to be retained.
    cleanup_poisoned: Arc<AtomicBool>,
}

impl TaskActor {
    /// Creates an actor for one accepted task registration.
    pub(crate) fn new(
        bus: Bus,
        name: Arc<str>,
        task: Arc<dyn Task>,
        params: TaskActorParams,
        resources: TaskActorResources,
        id: TaskId,
    ) -> Self {
        let TaskActorResources {
            semaphore,
            activity,
            cleanup_poisoned,
        } = resources;
        Self {
            id,
            name,
            task,
            params,
            bus,
            semaphore,
            activity,
            cleanup_poisoned,
        }
    }

    /// Runs attempts until policy, failure, or cancellation selects an exit.
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
                            Err(_closed) => return ActorExitReason::Canceled,
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
            let activity = AttemptActivity::begin(Arc::clone(&self.activity));

            self.bus.publish_lazy(|| {
                Event::new(EventKind::AttemptStarting)
                    .with_task(task_name.clone())
                    .with_id(id)
                    .with_attempt(attempt)
            });
            let attempt_start = Instant::now();
            let permit_guard = permit;
            let activity_guard = activity;
            let result = run_once(
                self.task.as_ref(),
                &task_name,
                AttemptRun {
                    parent: &runtime_token,
                    timeout: self.params.timeout,
                    attempt,
                    id,
                    bus: &self.bus,
                    cleanup_poisoned: Arc::clone(&self.cleanup_poisoned),
                },
            )
            .await;
            let decision = classify_attempt(
                result,
                self.params.restart,
                self.params.max_retries,
                backoff_attempt,
                self.cleanup_poisoned.as_ref(),
            );
            drop(activity_guard);
            drop(permit_guard);

            match decision {
                AttemptDecision::CleanupPanicked => {
                    return ActorExitReason::Panicked {
                        cleanup_poisoned: self.cleanup_poisoned.load(Ordering::Acquire),
                    };
                }
                AttemptDecision::Retry { reason } => {
                    let delay = self.params.backoff.delay_for_retry(backoff_attempt);
                    backoff_attempt = backoff_attempt.saturating_add(1);

                    self.bus.publish_lazy(|| {
                        Event::new(EventKind::BackoffScheduled)
                            .with_backoff_failure()
                            .with_task(task_name.clone())
                            .with_id(id)
                            .with_delay(delay)
                            .with_attempt(attempt)
                            .with_reason(reason)
                    });
                    if !Self::sleep_cancellable(delay, &runtime_token).await {
                        return ActorExitReason::Canceled;
                    }
                }
                AttemptDecision::Finished(Ok(())) => {
                    backoff_attempt = 0;

                    match self.params.restart {
                        RestartPolicy::Always { interval } => {
                            if let Some(d) = interval {
                                let delay = floored_interval(d, attempt_start.elapsed());
                                self.bus.publish_lazy(|| {
                                    Event::new(EventKind::BackoffScheduled)
                                        .with_backoff_success()
                                        .with_task(task_name.clone())
                                        .with_id(id)
                                        .with_attempt(attempt)
                                        .with_delay(delay)
                                });
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
                            return ActorExitReason::Completed;
                        }
                    }
                }
                AttemptDecision::Finished(Err(failure)) if failure.error.is_fatal() => {
                    let AttemptFailure {
                        error,
                        reason,
                        exit_code,
                        ..
                    } = failure;
                    let source: Option<SharedError> = error.into_source().map(Arc::from);

                    return ActorExitReason::Fatal {
                        reason,
                        exit_code,
                        source,
                    };
                }
                AttemptDecision::Finished(Err(failure))
                    if matches!(&failure.error, TaskError::Canceled) =>
                {
                    return ActorExitReason::Canceled;
                }
                AttemptDecision::Finished(Err(failure)) => {
                    let retries_exhausted = self
                        .params
                        .max_retries
                        .is_some_and(|max| backoff_attempt >= max.get());

                    let AttemptFailure {
                        error,
                        reason: attempt_reason,
                        exit_code,
                        ..
                    } = failure;
                    let reason: Arc<str> = if let Some(limit) =
                        self.params.max_retries.filter(|_| retries_exhausted)
                    {
                        Arc::from(format!(
                            "retry limit reached after {backoff_attempt} of {} retries: {attempt_reason}",
                            limit.get()
                        ))
                    } else {
                        attempt_reason
                    };
                    let source: Option<SharedError> = error.into_source().map(Arc::from);

                    return ActorExitReason::Exhausted {
                        reason,
                        exit_code,
                        source,
                    };
                }
            }
        }
    }

    /// Returns whether a delay completed before cancellation.
    #[inline]
    async fn sleep_cancellable(duration: Duration, token: &CancellationToken) -> bool {
        if token.is_cancelled() {
            return false;
        }
        if duration.is_zero() {
            tokio::task::yield_now().await;
            return !token.is_cancelled();
        }
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

    fn actor(
        name: &'static str,
        task: Arc<dyn Task>,
        restart: RestartPolicy,
        max_retries: u32,
    ) -> TaskActor {
        TaskActor::new(
            Bus::new(16),
            Arc::from(name),
            Arc::clone(&task),
            params(restart, max_retries),
            TaskActorResources {
                semaphore: None,
                activity: Arc::new(AtomicBool::new(false)),
                cleanup_poisoned: Arc::new(AtomicBool::new(false)),
            },
            TaskId::next(),
        )
    }

    struct OkTask;
    impl Task for OkTask {
        fn spawn(&self, _ctx: TaskContext) -> BoxFut {
            Box::pin(async { Ok(()) })
        }
    }

    struct FailTask;
    impl Task for FailTask {
        fn spawn(&self, _ctx: TaskContext) -> BoxFut {
            Box::pin(async { Err(TaskError::fail("boom")) })
        }
    }

    struct FatalTask;
    impl Task for FatalTask {
        fn spawn(&self, _ctx: TaskContext) -> BoxFut {
            Box::pin(async { Err(TaskError::fatal("fatal")) })
        }
    }

    struct PanickingDropFuture;

    impl Future for PanickingDropFuture {
        type Output = Result<(), TaskError>;

        fn poll(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            std::task::Poll::Pending
        }
    }

    impl Drop for PanickingDropFuture {
        fn drop(&mut self) {
            panic!("future cleanup panic");
        }
    }

    struct TimeoutCleanupPanicTask {
        attempts: AtomicU32,
    }

    struct NestedPanicPayload;

    impl Drop for NestedPanicPayload {
        fn drop(&mut self) {
            panic!("nested payload destructor panic");
        }
    }

    struct NestedPayloadFuture;

    impl Future for NestedPayloadFuture {
        type Output = Result<(), TaskError>;

        fn poll(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            std::panic::panic_any(NestedPanicPayload)
        }
    }

    struct NestedPayloadTask {
        attempts: AtomicU32,
    }

    impl Task for NestedPayloadTask {
        fn spawn(&self, _ctx: TaskContext) -> BoxFut {
            self.attempts.fetch_add(1, Ordering::AcqRel);
            Box::pin(NestedPayloadFuture)
        }
    }

    impl Task for TimeoutCleanupPanicTask {
        fn spawn(&self, _ctx: TaskContext) -> BoxFut {
            self.attempts.fetch_add(1, Ordering::AcqRel);
            Box::pin(PanickingDropFuture)
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
            let a = actor("ok", Arc::new(OkTask), restart, 0);
            let reason = a.run(CancellationToken::new()).await;
            assert!(
                matches!(reason, ActorExitReason::Completed),
                "{restart:?} + Ok task must exit Completed, got {reason:?}"
            );
        }
    }

    #[tokio::test]
    async fn fatal_error_returns_fatal_with_reason() {
        let a = actor("fatal", Arc::new(FatalTask), RestartPolicy::OnFailure, 0);
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
        let a = actor("fail", Arc::new(FailTask), RestartPolicy::OnFailure, 3);
        let reason = a.run(CancellationToken::new()).await;
        match reason {
            ActorExitReason::Exhausted { reason, .. } => {
                assert!(reason.contains("retry limit reached"));
            }
            other => panic!("expected Exhausted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancellation_returns_cancelled() {
        let token = CancellationToken::new();
        token.cancel();
        let a = actor(
            "ok",
            Arc::new(OkTask),
            RestartPolicy::Always { interval: None },
            0,
        );
        let reason = a.run(token).await;
        assert!(matches!(reason, ActorExitReason::Canceled));
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_cleanup_panic_is_terminal_even_with_unlimited_retries() {
        let task = Arc::new(TimeoutCleanupPanicTask {
            attempts: AtomicU32::new(0),
        });
        let actor = TaskActor::new(
            Bus::new(16),
            Arc::from("timeout-cleanup-panic"),
            Arc::clone(&task) as Arc<dyn Task>,
            TaskActorParams {
                restart: RestartPolicy::OnFailure,
                backoff: fast_backoff(),
                timeout: Some(Duration::from_millis(10)),
                max_retries: None,
            },
            TaskActorResources {
                semaphore: None,
                activity: Arc::new(AtomicBool::new(false)),
                cleanup_poisoned: Arc::new(AtomicBool::new(false)),
            },
            TaskId::next(),
        );

        let reason = actor.run(CancellationToken::new()).await;
        assert!(matches!(
            reason,
            ActorExitReason::Panicked {
                cleanup_poisoned: false
            }
        ));
        assert_eq!(
            task.attempts.load(Ordering::Acquire),
            1,
            "a panicking future destructor must terminalize the actor, not retry"
        );
    }

    #[tokio::test]
    async fn nested_panic_payload_destructor_poison_is_propagated() {
        let task = Arc::new(NestedPayloadTask {
            attempts: AtomicU32::new(0),
        });
        let actor = TaskActor::new(
            Bus::new(16),
            Arc::from("nested-payload"),
            Arc::clone(&task) as Arc<dyn Task>,
            TaskActorParams {
                restart: RestartPolicy::OnFailure,
                backoff: fast_backoff(),
                timeout: None,
                max_retries: None,
            },
            TaskActorResources {
                semaphore: None,
                activity: Arc::new(AtomicBool::new(false)),
                cleanup_poisoned: Arc::new(AtomicBool::new(false)),
            },
            TaskId::next(),
        );

        let reason = actor.run(CancellationToken::new()).await;
        assert!(matches!(
            reason,
            ActorExitReason::Panicked {
                cleanup_poisoned: true
            }
        ));
        assert_eq!(task.attempts.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn on_failure_retries_then_succeeds() {
        let task = Arc::new(CountedTask::new(2));
        let a = actor("counted", task, RestartPolicy::OnFailure, 0);
        let reason = a.run(CancellationToken::new()).await;
        assert!(matches!(reason, ActorExitReason::Completed));
    }

    #[tokio::test(start_paused = true)]
    async fn always_none_instant_ok_is_rate_limited() {
        use std::sync::atomic::{AtomicU32, Ordering};

        struct Counting(Arc<AtomicU32>);
        impl Task for Counting {
            fn spawn(&self, _ctx: TaskContext) -> BoxFut {
                self.0.fetch_add(1, Ordering::Relaxed);
                Box::pin(async { Ok(()) })
            }
        }

        let counter = Arc::new(AtomicU32::new(0));
        let task = Arc::new(Counting(Arc::clone(&counter)));
        let a = actor("spin", task, RestartPolicy::Always { interval: None }, 0);

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
