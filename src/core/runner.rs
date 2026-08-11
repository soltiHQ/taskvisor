//! # Run one task attempt
//!
//! [`run_once`] calls [`Task::spawn`], applies one attempt timeout, catches panics while calling or polling the task, and publishes attempt events.
//! It returns the attempt result to [`TaskActor`](super::actor::TaskActor), which decides whether to restart.
//!
//! ## Event Flow
//!
//! | Attempt result                                      | Events                          | Returned result      |
//! |-----------------------------------------------------|---------------------------------|----------------------|
//! | `Ok(())`                                            | `AttemptSucceeded`              | `Ok(())`             |
//! | `TaskError::Canceled`                               | `AttemptCanceled`               | Same error           |
//! | Task-returned `Fail`, `Fatal`, or `Timeout`         | `AttemptFailed`                 | Same error           |
//! | Panic while calling `spawn()` or polling its future | `AttemptFailed`                 | `TaskError::Fail`    |
//! | Configured attempt timer expires                    | `AttemptTimedOut`               | `TaskError::Timeout` |
//!
//! ## Rules
//!
//! - Each completed call publishes one final attempt event: `AttemptSucceeded`, `AttemptCanceled`, `AttemptFailed`, or `AttemptTimedOut`.
//!   Force-aborting the managed runner can drop an in-flight call before that event.
//! - Each attempt gets a child cancellation token. Parent cancellation reaches it, but child cancellation does not affect the parent.
//! - Panics while calling `spawn()` or polling its future become retryable [`TaskError::Fail`] values.
//! - `AttemptTimedOut` is published only when the configured attempt timer expires.
//! - `TaskError::Canceled` is a cooperative stop, not a failure.

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

/// Catches panics while polling a task future.
///
/// A panic is converted to a retryable [`TaskError::Fail`].
/// This keeps user panics on the normal failure path instead of unwinding through the actor.
struct CaughtFailure {
    error: TaskError,
    cleanup_panicked: bool,
}

struct CatchPanic {
    future: Option<BoxTaskFuture>,
    cleanup_poisoned: Arc<AtomicBool>,
}

impl CatchPanic {
    fn new(future: BoxTaskFuture, cleanup_poisoned: Arc<AtomicBool>) -> Self {
        Self {
            future: Some(future),
            cleanup_poisoned,
        }
    }

    /// Destroys one user future inside the physical attempt boundary.
    ///
    /// `Future::drop` is synchronous and can block. Keeping it here means the
    /// attempt still owns its concurrency permit and activity bit until that
    /// destructor really returns. A destructor panic becomes an attempt failure;
    /// a second panic from destroying its payload is intentionally retained.
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

    /// Explicitly destroys an in-flight future while the attempt still owns its
    /// permit. Timeout uses this path so a destructor panic cannot be mistaken
    /// for an ordinary, retryable timeout.
    fn dispose(self: Pin<&mut Self>) -> Option<CaughtFailure> {
        let this = self.get_mut();
        let future = this.future.take()?;
        Self::drop_future(future, this.cleanup_poisoned.as_ref())
    }

    fn dispose_value<T>(value: T, cleanup_poisoned: &AtomicBool) {
        if let Err(payload) = std::panic::catch_unwind(AssertUnwindSafe(|| drop(value))) {
            dispose_panic_payload(payload, cleanup_poisoned);
        }
    }
}

impl Future for CatchPanic {
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
                        // A returned TaskError can itself retain user values. If
                        // future cleanup already failed, destroy that result under
                        // the same physical attempt boundary before terminalizing.
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
                // Preserve the polling panic as the primary attempt failure.
                // Destruction is still isolated from unwinding so a hostile Drop
                // cannot cause a double panic in this boundary.
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

impl Drop for CatchPanic {
    fn drop(&mut self) {
        if let Some(future) = self.future.take() {
            // Timeout, cancellation, and force-abort all reach this path. The
            // enclosing attempt continues to own its permit/activity while the
            // synchronous destructor executes.
            let _drop_error = Self::drop_future(future, self.cleanup_poisoned.as_ref());
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

/// Destroys a panic payload without allowing a hostile payload destructor to
/// unwind through the attempt boundary.
pub(crate) fn dispose_panic_payload(
    payload: Box<dyn std::any::Any + Send>,
    cleanup_poisoned: &AtomicBool,
) -> bool {
    if let Err(nested) = std::panic::catch_unwind(AssertUnwindSafe(|| drop(payload))) {
        // No bounded execution context can safely run a destructor that has
        // already panicked while being destroyed. Retaining the nested payload
        // prevents a double panic and keeps this physical attempt well-defined.
        cleanup_poisoned.store(true, Ordering::Release);
        std::mem::forget(nested);
        true
    } else {
        false
    }
}

/// One failed attempt with diagnostics computed exactly once.
///
/// The runner publishes the attempt event and the actor later decides whether
/// to retry or terminate. Carrying the formatted reason across that boundary
/// avoids formatting the same user error on both paths.
#[derive(Debug)]
pub(crate) struct AttemptFailure {
    pub(crate) error: TaskError,
    pub(crate) reason: Arc<str>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) cleanup_panicked: bool,
}

/// Registry resources and metadata for one physical attempt.
pub(crate) struct AttemptRun<'a> {
    pub(crate) parent: &'a CancellationToken,
    pub(crate) timeout: Option<Duration>,
    pub(crate) attempt: u32,
    pub(crate) id: TaskId,
    pub(crate) bus: &'a Bus,
    pub(crate) cleanup_poisoned: Arc<AtomicBool>,
}

impl AttemptFailure {
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

    fn caught(failure: CaughtFailure) -> Self {
        let mut attempt = Self::new(failure.error);
        attempt.cleanup_panicked = failure.cleanup_panicked;
        attempt
    }
}

/// Runs one attempt and publishes its events.
///
/// The actor receives the raw result and applies restart and backoff rules.
///
/// ### Steps
///
/// 1. Create a child cancellation token.
/// 2. Call [`Task::spawn`] and catch panics while calling or polling it.
/// 3. Run the future with the optional timeout.
/// 4. Publish the attempt event.
/// 5. Return the attempt result.
///
/// ### Timeout
///
/// A positive timeout limits this attempt only.
/// When the configured timer expires, the attempt future is dropped and is no longer polled.
/// The child token is then cancelled so work that cloned it can observe cancellation.
/// A configured timeout publishes `AttemptTimedOut` as the attempt's single terminal event.
/// A task that explicitly returns `TaskError::Timeout` instead follows the ordinary `AttemptFailed` path.
/// `None` and zero mean no timeout.
///
/// ### Cancellation
///
/// Parent cancellation reaches the attempt context.
/// A cooperative task should observe [`TaskContext::cancelled`](crate::TaskContext::cancelled) and return [`TaskError::Canceled`].
/// This publishes `AttemptCanceled`, not `AttemptFailed`.
///
/// ### Panic Handling
///
/// Panics from `spawn()` or from polling its future become retryable [`TaskError::Fail`] values with reason `task panicked: ...`.
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
        Ok(fut) => CatchPanic::new(fut, Arc::clone(&cleanup_poisoned)),
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
        fn name(&self) -> &str {
            "slow-task"
        }

        fn spawn(&self, _ctx: TaskContext) -> BoxFut {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok(())
            })
        }
    }

    struct FailTask;

    impl Task for FailTask {
        fn name(&self) -> &str {
            "fail-task"
        }

        fn spawn(&self, _ctx: TaskContext) -> BoxFut {
            Box::pin(async { Err(TaskError::fail("boom")) })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_returns_timeout_and_publishes_attempt_timed_out() {
        let bus = Bus::new(16);
        let mut rx = bus.subscribe();
        let parent = CancellationToken::new();
        let timeout = Some(Duration::from_millis(50));

        let result = run_once(
            &SlowTask,
            &Arc::from(SlowTask.name()),
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
            fn name(&self) -> &str {
                "sleep-ok"
            }
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
            &Arc::from(SleepOk.name()),
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
            &Arc::from(FailTask.name()),
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
}
