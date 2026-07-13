//! # Run one task attempt
//!
//! [`run_once`] calls [`Task::spawn`], applies one attempt timeout, catches user-task panics, and publishes attempt events.
//! It returns the attempt result to [`TaskActor`](super::actor::TaskActor), which decides whether to restart.
//!
//! ## Event Flow
//!
//! ```text
//! Ok(())
//!   -> TaskStopped
//!   -> return Ok(())
//!
//! Err(TaskError::Canceled)
//!   -> TaskCanceled
//!   -> return Err(Canceled)
//!
//! Err(TaskError::Fail | TaskError::Fatal)
//!   -> TaskFailed
//!   -> return the same error
//!
//! panic in spawn() or the task future
//!   -> TaskFailed(reason="task panicked: ...")
//!   -> return Err(TaskError::Fail)
//!
//! timeout
//!   -> cancel child token
//!   -> TimeoutHit
//!   -> TaskFailed
//!   -> return Err(TaskError::Timeout)
//! ```
//!
//! ## Rules
//!
//! - Each call publishes one final attempt event: `TaskStopped`, `TaskCanceled`, or `TaskFailed`.
//! - `TimeoutHit` adds context before the final `TaskFailed` event.
//! - `TaskError::Canceled` is a cooperative stop, not a failure.
//! - Each attempt gets a child cancellation token. Parent cancellation reaches it, but child cancellation does not affect the parent.
//! - User-task panics become retryable [`TaskError::Fail`] values.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
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
struct CatchPanic(BoxTaskFuture);

impl Future for CatchPanic {
    type Output = Result<(), TaskError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        std::panic::catch_unwind(AssertUnwindSafe(|| self.0.as_mut().poll(cx)))
            .unwrap_or_else(|payload| Poll::Ready(Err(panic_to_error(payload.as_ref()))))
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

/// Runs one attempt and publishes its events.
///
/// The actor receives the raw result and applies restart and backoff rules.
///
/// ### Steps
///
/// 1. Create a child cancellation token.
/// 2. Call [`Task::spawn`] and catch panics.
/// 3. Run the future with the optional timeout.
/// 4. Publish the attempt event.
/// 5. Return the attempt result.
///
/// ### Timeout
///
/// A positive timeout limits this attempt only.
/// On timeout, the child token is cancelled, `TimeoutHit` is published, and the final event is `TaskFailed` with [`TaskError::Timeout`]. `None` and zero mean no timeout.
///
/// ### Cancellation
///
/// Parent cancellation reaches the attempt context.
/// A cooperative task should observe [`TaskContext::cancelled`](crate::TaskContext::cancelled) and return [`TaskError::Canceled`].
/// This publishes `TaskCanceled`, not `TaskFailed`.
///
/// ### Panic Handling
///
/// Panics from `spawn()` or from polling its future become retryable [`TaskError::Fail`] values with reason `task panicked: ...`.
pub async fn run_once<T: Task + ?Sized>(
    task: &T,
    parent: &CancellationToken,
    timeout: Option<Duration>,
    attempt: u32,
    id: TaskId,
    bus: &Bus,
) -> Result<(), TaskError> {
    let started = Instant::now();
    let child = parent.child_token();
    let ctx = TaskContext::from_token(child.clone());

    let fut = match std::panic::catch_unwind(AssertUnwindSafe(move || task.spawn(ctx))) {
        Ok(fut) => CatchPanic(fut),
        Err(payload) => {
            let e = panic_to_error(payload.as_ref());
            publish_failed(bus, id, task.name(), attempt, &e, started.elapsed());
            return Err(e);
        }
    };

    let res = if let Some(dur) = timeout.filter(|d| *d > Duration::ZERO) {
        match time::timeout(dur, fut).await {
            Ok(r) => r,
            Err(_elapsed) => {
                child.cancel();
                publish_timeout(bus, id, task.name(), dur, attempt, started.elapsed());
                Err(TaskError::timeout(dur))
            }
        }
    } else {
        fut.await
    };

    match res {
        Ok(()) => {
            publish_stopped(bus, id, task.name(), attempt, started.elapsed());
            Ok(())
        }
        Err(TaskError::Canceled) => {
            publish_canceled(bus, id, task.name(), attempt, started.elapsed());
            Err(TaskError::Canceled)
        }
        Err(e) => {
            publish_failed(bus, id, task.name(), attempt, &e, started.elapsed());
            Err(e)
        }
    }
}

/// Publishes `TaskStopped` for a successful attempt.
fn publish_stopped(bus: &Bus, id: TaskId, name: &str, attempt: u32, duration: Duration) {
    bus.publish(
        Event::new(EventKind::TaskStopped)
            .with_task(name)
            .with_id(id)
            .with_attempt(attempt)
            .with_duration(duration),
    );
}

/// Publishes `TaskCanceled` for a cooperative cancellation attempt.
fn publish_canceled(bus: &Bus, id: TaskId, name: &str, attempt: u32, duration: Duration) {
    bus.publish(
        Event::new(EventKind::TaskCanceled)
            .with_task(name)
            .with_id(id)
            .with_attempt(attempt)
            .with_duration(duration),
    );
}

/// Publishes `TaskFailed` with error details and attempt duration.
fn publish_failed(
    bus: &Bus,
    id: TaskId,
    name: &str,
    attempt: u32,
    err: &TaskError,
    duration: Duration,
) {
    let mut ev = Event::new(EventKind::TaskFailed)
        .with_task(name)
        .with_id(id)
        .with_attempt(attempt)
        .with_duration(duration)
        .with_reason(err.to_string());
    if let Some(code) = err.exit_code() {
        ev = ev.with_exit_code(code);
    }
    bus.publish(ev);
}

/// Publishes `TimeoutHit` before the final timeout `TaskFailed` event.
fn publish_timeout(
    bus: &Bus,
    id: TaskId,
    name: &str,
    dur: Duration,
    attempt: u32,
    duration: Duration,
) {
    bus.publish(
        Event::new(EventKind::TimeoutHit)
            .with_task(name)
            .with_id(id)
            .with_timeout(dur)
            .with_attempt(attempt)
            .with_duration(duration),
    );
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
    async fn timeout_returns_timeout_and_publishes_timeout_hit() {
        let bus = Bus::new(16);
        let mut rx = bus.subscribe();
        let parent = CancellationToken::new();
        let timeout = Some(Duration::from_millis(50));

        let result = run_once(&SlowTask, &parent, timeout, 1, TaskId::next(), &bus).await;

        match result {
            Err(TaskError::Timeout { timeout: dur }) => {
                assert_eq!(dur, Duration::from_millis(50));
            }
            Err(TaskError::Fail { reason, .. }) => {
                panic!("timeout should return TaskError::Timeout, not TaskError::Fail: {reason}");
            }
            other => {
                panic!("expected TaskError::Timeout, got: {other:?}");
            }
        }
        assert!(
            std::iter::from_fn(|| rx.try_recv().ok())
                .any(|event| event.kind == EventKind::TimeoutHit),
            "a timeout result must be accompanied by TimeoutHit"
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

        run_once(&SleepOk, &parent, None, 3, TaskId::next(), &bus)
            .await
            .expect("task succeeds");

        let stopped = std::iter::from_fn(|| rx.try_recv().ok())
            .find(|event| event.kind == EventKind::TaskStopped)
            .expect("a successful attempt must publish TaskStopped");
        assert_eq!(
            stopped.attempt,
            Some(3),
            "TaskStopped must carry the attempt number"
        );
        let measured = stopped
            .duration_ms
            .expect("TaskStopped must carry the attempt duration");
        assert!(
            measured >= 20,
            "attempt duration must reflect the ~30ms of work, got {measured}ms"
        );
    }

    #[tokio::test]
    async fn failure_returns_fail_variant() {
        let bus = Bus::new(16);
        let parent = CancellationToken::new();

        let result = run_once(&FailTask, &parent, None, 1, TaskId::next(), &bus).await;

        assert!(
            matches!(result, Err(TaskError::Fail { .. })),
            "expected TaskError::Fail, got: {result:?}"
        );
    }
}
