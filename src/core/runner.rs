//! Executes one physical task attempt.
//!
//! [`TaskActor`](super::actor::TaskActor) calls [`run_once`] after acquiring any concurrency permit.
//! Each attempt contains task polling, timeout handling, and user panics within one physical ownership boundary.
//! The runner publishes one terminal attempt event before returning the classified result.
//!
//! ```text
//! TaskActor ──► run_once
//!                  ├── success ──► AttemptSucceeded
//!                  ├── cancellation ──► AttemptCanceled
//!                  ├── configured timer ──► AttemptTimedOut
//!                  └── task error or panic ──► AttemptFailed
//! ```
//!
//! The attempt future is destroyed before its activity flag and concurrency permit are released.
//! This remains true during timeout and Tokio abort.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::time;
use tokio_util::sync::CancellationToken;

use crate::{
    error::TaskError,
    events::{Bus, Event, EventKind},
    identity::TaskId,
    tasks::{BoxTaskFuture, Task, TaskContext},
};

/// Failure returned by the task-future panic boundary.
///
/// Polling panics become [`TaskError::Fail`] values.
/// The cleanup flag records a second panic while destroying a user-owned value; that case is not retried.
struct CaughtFailure {
    /// Task error produced from a returned error or panic payload.
    error: TaskError,
    /// Whether destroying a user value also panicked.
    cleanup_panicked: bool,
}

/// Event context used when abort-time future cleanup panics.
struct DropDiagnostic<'a> {
    /// Runtime event bus.
    bus: &'a Bus,
    /// Stable task name.
    task_name: &'a Arc<str>,
    /// Runtime task identity.
    id: TaskId,
    /// Attempt number.
    attempt: u32,
}

impl DropDiagnostic<'_> {
    /// Publishes a best-effort cleanup-panic diagnostic.
    fn publish(&self, failure: &CaughtFailure) {
        self.bus.publish_lazy(|| {
            Event::runtime_failure(
                "task_runner",
                format!(
                    "future_drop_panicked task={}: {}",
                    self.task_name, failure.error
                ),
            )
            .with_id(self.id)
            .with_attempt(self.attempt)
        });
    }
}

/// Panic boundary around one user task future.
struct CatchPanic<'a> {
    /// User future present until completion or explicit disposal.
    future: Option<BoxTaskFuture>,
    /// Actor-level nested cleanup-panic flag.
    cleanup_poisoned: Arc<AtomicBool>,
    /// Diagnostic context used when `Drop` observes a cleanup panic.
    drop_diagnostic: DropDiagnostic<'a>,
}

impl<'a> CatchPanic<'a> {
    /// Wraps one task future in its physical attempt boundary.
    fn new(
        future: BoxTaskFuture,
        cleanup_poisoned: Arc<AtomicBool>,
        drop_diagnostic: DropDiagnostic<'a>,
    ) -> Self {
        Self {
            future: Some(future),
            cleanup_poisoned,
            drop_diagnostic,
        }
    }

    /// Destroys one user future inside the physical attempt boundary.
    ///
    /// `Future::drop` is synchronous and can block.
    /// The attempt retains its concurrency permit and activity bit until that destructor returns.
    /// The caller classifies a destructor panic as an attempt failure or an abort-time runtime diagnostic.
    /// A second panic from destroying its payload is intentionally retained.
    fn drop_future(future: BoxTaskFuture, cleanup_poisoned: &AtomicBool) -> Option<CaughtFailure> {
        match std::panic::catch_unwind(AssertUnwindSafe(|| drop(future))) {
            Ok(()) => None,
            Err(payload) => {
                let error = panic_to_error(payload.as_ref());
                dispose_panic_payload(payload, cleanup_poisoned);
                Some(CaughtFailure {
                    error,
                    cleanup_panicked: true,
                })
            }
        }
    }

    /// In-flight future cleanup while the attempt still owns its permit.
    /// Timeout uses this path to keep destructor panic distinct from an ordinary retryable timeout.
    fn dispose(self: Pin<&mut Self>) -> Option<CaughtFailure> {
        let this = self.get_mut();
        let future = this.future.take()?;
        Self::drop_future(future, this.cleanup_poisoned.as_ref())
    }

    /// Destroys a returned user value without allowing its panic to escape.
    fn dispose_value<T>(value: T, cleanup_poisoned: &AtomicBool) {
        if let Err(payload) = std::panic::catch_unwind(AssertUnwindSafe(|| drop(value))) {
            dispose_panic_payload(payload, cleanup_poisoned);
        }
    }
}

impl Future for CatchPanic<'_> {
    type Output = Result<(), CaughtFailure>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let future = self
            .future
            .as_mut()
            .expect("the task future is present until its attempt finishes");
        match std::panic::catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(result)) => {
                let future = self
                    .future
                    .take()
                    .expect("a ready task future is destroyed exactly once");
                match Self::drop_future(future, self.cleanup_poisoned.as_ref()) {
                    Some(failure) => {
                        Self::dispose_value(result, self.cleanup_poisoned.as_ref());
                        Poll::Ready(Err(failure))
                    }
                    None => Poll::Ready(result.map_err(|error| CaughtFailure {
                        error,
                        cleanup_panicked: false,
                    })),
                }
            }
            Err(payload) => {
                let error = panic_to_error(payload.as_ref());
                let payload_poisoned =
                    dispose_panic_payload(payload, self.cleanup_poisoned.as_ref());
                let future = self
                    .future
                    .take()
                    .expect("a panicked task future is destroyed exactly once");
                let cleanup_panicked =
                    Self::drop_future(future, self.cleanup_poisoned.as_ref()).is_some();
                Poll::Ready(Err(CaughtFailure {
                    error,
                    cleanup_panicked: payload_poisoned || cleanup_panicked,
                }))
            }
        }
    }
}

impl Drop for CatchPanic<'_> {
    fn drop(&mut self) {
        if let Some(future) = self.future.take()
            && let Some(failure) = Self::drop_future(future, self.cleanup_poisoned.as_ref())
        {
            self.drop_diagnostic.publish(&failure);
        }
    }
}

/// Converts a panic payload into a retryable [`TaskError::Fail`].
fn panic_to_error(payload: &(dyn std::any::Any + Send)) -> TaskError {
    let msg = payload
        .downcast_ref::<&'static str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload");
    TaskError::fail(format!("task panicked: {msg}"))
}

/// Destroys a panic payload inside the attempt boundary.
///
/// A payload-destructor panic that must be retained produces `true`.
pub(crate) fn dispose_panic_payload(
    payload: Box<dyn std::any::Any + Send>,
    cleanup_poisoned: &AtomicBool,
) -> bool {
    if let Err(nested) = std::panic::catch_unwind(AssertUnwindSafe(|| drop(payload))) {
        cleanup_poisoned.store(true, Ordering::Release);
        std::mem::forget(nested);
        true
    } else {
        false
    }
}

/// One failed attempt with diagnostics computed exactly once.
///
/// The runner publishes the attempt event and the actor later decides whether to retry or terminate.
/// Carrying the formatted reason across that boundary avoids formatting the same user error on both paths.
#[derive(Debug)]
pub(crate) struct AttemptFailure {
    /// Original classified task error.
    pub(crate) error: TaskError,
    /// Formatted diagnostic text reused by actor and events.
    pub(crate) reason: Arc<str>,
    /// Process-like exit code, when present.
    pub(crate) exit_code: Option<i32>,
    /// Whether user-value cleanup also panicked.
    pub(crate) cleanup_panicked: bool,
}

/// Inputs passed from the task actor to one physical attempt.
pub(crate) struct AttemptRun<'a> {
    /// Parent token that propagates runtime or task cancellation.
    pub(crate) parent: &'a CancellationToken,
    /// Optional attempt deadline.
    pub(crate) timeout: Option<Duration>,
    /// One-based attempt number.
    pub(crate) attempt: u32,
    /// Registered task identity.
    pub(crate) id: TaskId,
    /// Event bus used for attempt events.
    pub(crate) bus: &'a Bus,
    /// Actor-level nested cleanup-panic flag.
    pub(crate) cleanup_poisoned: Arc<AtomicBool>,
}

impl AttemptFailure {
    /// Classifies one task error for actor and event consumers.
    fn new(error: TaskError) -> Self {
        let reason = Arc::from(error.to_string());
        let exit_code = error.exit_code();
        Self {
            error,
            reason,
            exit_code,
            cleanup_panicked: false,
        }
    }

    /// Converts a panic-boundary failure into an attempt failure.
    fn caught(failure: CaughtFailure) -> Self {
        let mut attempt = Self::new(failure.error);
        attempt.cleanup_panicked = failure.cleanup_panicked;
        attempt
    }
}

/// One physical attempt with a classified result for the task actor.
///
/// A positive timeout applies only to this attempt.
/// Expiry cancels and destroys the attempt future before returning [`TaskError::Timeout`].
/// A timeout returned by the task follows the ordinary failure path.
/// Panics from `spawn` or polling become attempt failures.
/// A cleanup panic makes the actor stop instead of retrying.
pub(crate) async fn run_once<T: Task + ?Sized>(
    task: &T,
    task_name: &Arc<str>,
    run: AttemptRun<'_>,
) -> Result<(), AttemptFailure> {
    let AttemptRun {
        parent,
        timeout,
        attempt,
        id,
        bus,
        cleanup_poisoned,
    } = run;
    let started = Instant::now();
    let child = parent.child_token();
    let ctx = TaskContext::from_token(child.clone());

    let fut = match std::panic::catch_unwind(AssertUnwindSafe(move || task.spawn(ctx))) {
        Ok(fut) => CatchPanic::new(
            fut,
            Arc::clone(&cleanup_poisoned),
            DropDiagnostic {
                bus,
                task_name,
                id,
                attempt,
            },
        ),
        Err(payload) => {
            let mut failure = AttemptFailure::new(panic_to_error(payload.as_ref()));
            failure.cleanup_panicked = dispose_panic_payload(payload, cleanup_poisoned.as_ref());
            publish_failed(bus, id, task_name, attempt, &failure, started.elapsed());
            return Err(failure);
        }
    };

    let res = if let Some(dur) = timeout.filter(|d| *d > Duration::ZERO) {
        tokio::pin!(fut);
        let timer = time::sleep(dur);
        tokio::pin!(timer);
        tokio::select! {
            result = &mut fut => result,
            _ = &mut timer => {
                child.cancel();
                if let Some(cleanup_failure) = fut.as_mut().dispose() {
                    let failure = AttemptFailure::caught(cleanup_failure);
                    publish_failed(bus, id, task_name, attempt, &failure, started.elapsed());
                    return Err(failure);
                }
                publish_timeout(bus, id, task_name, dur, attempt, started.elapsed());
                return Err(AttemptFailure::new(TaskError::timeout(dur)));
            }
        }
    } else {
        fut.await
    };

    match res {
        Ok(()) => {
            publish_stopped(bus, id, task_name, attempt, started.elapsed());
            Ok(())
        }
        Err(CaughtFailure {
            error: TaskError::Canceled,
            cleanup_panicked,
        }) => {
            publish_canceled(bus, id, task_name, attempt, started.elapsed());
            let mut failure = AttemptFailure::new(TaskError::Canceled);
            failure.cleanup_panicked = cleanup_panicked;
            Err(failure)
        }
        Err(failure) => {
            let failure = AttemptFailure::caught(failure);
            publish_failed(bus, id, task_name, attempt, &failure, started.elapsed());
            Err(failure)
        }
    }
}

/// Publishes `AttemptSucceeded` for a successful attempt.
fn publish_stopped(bus: &Bus, id: TaskId, name: &Arc<str>, attempt: u32, duration: Duration) {
    bus.publish_lazy(|| {
        Event::new(EventKind::AttemptSucceeded)
            .with_task(Arc::clone(name))
            .with_id(id)
            .with_attempt(attempt)
            .with_duration(duration)
    });
}

/// Publishes `AttemptCanceled` for a cooperative cancellation attempt.
fn publish_canceled(bus: &Bus, id: TaskId, name: &Arc<str>, attempt: u32, duration: Duration) {
    bus.publish_lazy(|| {
        Event::new(EventKind::AttemptCanceled)
            .with_task(Arc::clone(name))
            .with_id(id)
            .with_attempt(attempt)
            .with_duration(duration)
    });
}

/// Publishes `AttemptFailed` with error details and attempt duration.
fn publish_failed(
    bus: &Bus,
    id: TaskId,
    name: &Arc<str>,
    attempt: u32,
    failure: &AttemptFailure,
    duration: Duration,
) {
    bus.publish_lazy(|| {
        let mut event = Event::new(EventKind::AttemptFailed)
            .with_task(Arc::clone(name))
            .with_id(id)
            .with_attempt(attempt)
            .with_duration(duration)
            .with_reason(Arc::clone(&failure.reason));
        if let Some(code) = failure.exit_code {
            event = event.with_exit_code(code);
        }
        event
    });
}

/// Publishes `AttemptTimedOut` as the configured timeout's terminal attempt event.
fn publish_timeout(
    bus: &Bus,
    id: TaskId,
    name: &Arc<str>,
    dur: Duration,
    attempt: u32,
    duration: Duration,
) {
    bus.publish_lazy(|| {
        Event::new(EventKind::AttemptTimedOut)
            .with_task(Arc::clone(name))
            .with_id(id)
            .with_timeout(dur)
            .with_attempt(attempt)
            .with_duration(duration)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;

    type BoxFut = Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + 'static>>;

    struct SlowTask;

    impl Task for SlowTask {
        fn spawn(&self, _ctx: TaskContext) -> BoxFut {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok(())
            })
        }
    }

    struct FailTask;

    impl Task for FailTask {
        fn spawn(&self, _ctx: TaskContext) -> BoxFut {
            Box::pin(async { Err(TaskError::fail("boom")) })
        }
    }

    struct PendingDropFuture {
        polled: Arc<tokio::sync::Notify>,
        panic_on_drop: bool,
    }

    impl Future for PendingDropFuture {
        type Output = Result<(), TaskError>;

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.polled.notify_one();
            Poll::Pending
        }
    }

    impl Drop for PendingDropFuture {
        fn drop(&mut self) {
            if self.panic_on_drop {
                panic!("future cleanup panic");
            }
        }
    }

    async fn abort_pending_attempt(panic_on_drop: bool) -> (Vec<Arc<Event>>, TaskId) {
        let bus = Bus::new(16);
        let mut events = bus.subscribe();
        let polled = Arc::new(tokio::sync::Notify::new());
        let task_polled = Arc::clone(&polled);
        let id = TaskId::next();
        let task_name: Arc<str> = Arc::from(if panic_on_drop {
            "pending-drop-panic"
        } else {
            "pending-normal-drop"
        });
        let runner = tokio::spawn(async move {
            let task = crate::TaskFn::new(move |_ctx| PendingDropFuture {
                polled: Arc::clone(&task_polled),
                panic_on_drop,
            });
            let parent = CancellationToken::new();
            run_once(
                &task,
                &task_name,
                AttemptRun {
                    parent: &parent,
                    timeout: None,
                    attempt: 7,
                    id,
                    bus: &bus,
                    cleanup_poisoned: Arc::new(AtomicBool::new(false)),
                },
            )
            .await
        });

        polled.notified().await;
        runner.abort();
        let join_error = runner
            .await
            .expect_err("an explicitly aborted pending runner cannot complete naturally");
        assert!(
            join_error.is_cancelled(),
            "the future destructor panic must stay inside the runner boundary: {join_error}"
        );

        let drained = std::iter::from_fn(|| events.try_recv().ok()).collect();
        (drained, id)
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_returns_timeout_and_publishes_attempt_timed_out() {
        let bus = Bus::new(16);
        let mut rx = bus.subscribe();
        let parent = CancellationToken::new();
        let timeout = Some(Duration::from_millis(50));

        let result = run_once(
            &SlowTask,
            &Arc::from("slow-task"),
            AttemptRun {
                parent: &parent,
                timeout,
                attempt: 1,
                id: TaskId::next(),
                bus: &bus,
                cleanup_poisoned: Arc::new(AtomicBool::new(false)),
            },
        )
        .await;

        match result {
            Err(AttemptFailure {
                error: TaskError::Timeout { timeout: dur },
                ..
            }) => {
                assert_eq!(dur, Duration::from_millis(50));
            }
            Err(AttemptFailure {
                error: TaskError::Fail { reason, .. },
                ..
            }) => {
                panic!("timeout should return TaskError::Timeout, not TaskError::Fail: {reason}");
            }
            other => {
                panic!("expected TaskError::Timeout, got: {other:?}");
            }
        }
        assert!(
            std::iter::from_fn(|| rx.try_recv().ok())
                .any(|event| event.kind == EventKind::AttemptTimedOut),
            "a timeout result must be accompanied by AttemptTimedOut"
        );
    }

    #[tokio::test]
    async fn success_returns_ok_and_publishes_measured_stopped_event() {
        struct SleepOk;
        impl Task for SleepOk {
            fn spawn(&self, _ctx: TaskContext) -> BoxFut {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    Ok(())
                })
            }
        }

        let bus = Bus::new(16);
        let mut rx = bus.subscribe();
        let parent = CancellationToken::new();

        run_once(
            &SleepOk,
            &Arc::from("sleep-ok"),
            AttemptRun {
                parent: &parent,
                timeout: None,
                attempt: 3,
                id: TaskId::next(),
                bus: &bus,
                cleanup_poisoned: Arc::new(AtomicBool::new(false)),
            },
        )
        .await
        .expect("task succeeds");

        let stopped = std::iter::from_fn(|| rx.try_recv().ok())
            .find(|event| event.kind == EventKind::AttemptSucceeded)
            .expect("a successful attempt must publish AttemptSucceeded");
        assert_eq!(
            stopped.attempt,
            Some(3),
            "AttemptSucceeded must carry the attempt number"
        );
        let measured = stopped
            .duration_ms
            .expect("AttemptSucceeded must carry the attempt duration");
        assert!(
            measured >= 20,
            "attempt duration must reflect the ~30ms of work, got {measured}ms"
        );
    }

    #[tokio::test]
    async fn failure_returns_fail_variant() {
        let bus = Bus::new(16);
        let parent = CancellationToken::new();
        let result = run_once(
            &FailTask,
            &Arc::from("fail-task"),
            AttemptRun {
                parent: &parent,
                timeout: None,
                attempt: 1,
                id: TaskId::next(),
                bus: &bus,
                cleanup_poisoned: Arc::new(AtomicBool::new(false)),
            },
        )
        .await;

        assert!(
            matches!(
                result,
                Err(AttemptFailure {
                    error: TaskError::Fail { .. },
                    ..
                })
            ),
            "expected TaskError::Fail, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn aborted_future_drop_panic_publishes_only_runtime_diagnostic() {
        let (normal_events, _) = abort_pending_attempt(false).await;
        assert!(
            normal_events.is_empty(),
            "ordinary abort must not report a failure: {normal_events:?}"
        );

        let (panic_events, id) = abort_pending_attempt(true).await;
        assert_eq!(
            panic_events.len(),
            1,
            "abort-time cleanup panic must emit one diagnostic, not an attempt result: {panic_events:?}"
        );
        let diagnostic = &panic_events[0];
        assert_eq!(diagnostic.kind, EventKind::RuntimeFailure);
        assert_eq!(diagnostic.task.as_deref(), Some("task_runner"));
        assert_eq!(diagnostic.id, Some(id));
        assert_eq!(diagnostic.attempt, Some(7));
        assert!(diagnostic.reason.as_deref().is_some_and(|reason| {
            reason.contains("future_drop_panicked")
                && reason.contains("pending-drop-panic")
                && reason.contains("future cleanup panic")
        }));
    }
}
