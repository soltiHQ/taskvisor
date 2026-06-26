//! # Run a single attempt of a task execution.
//!
//! Executes one attempt of a [`Task`] with optional timeout, publishes lifecycle events to [`Bus`].
//! - **Apply timeout** if configured (wraps execution in `tokio::time::timeout`)
//! - **Execute ONE attempt** of the task with child cancellation token
//! - **Publish events** for observability (stopped/failed/timeout)
//!
//! ## Event flow
//!
//! ```text
//! Success:
//!   task.spawn() ─► Ok(()) ─► publish TaskStopped
//!
//! Cancellation:
//!   task.spawn() ─► Err(Canceled) ─► publish TaskCanceled (graceful exit)
//!
//! Failure:
//!   task.spawn() ─► Err(Fail/Fatal) ─► publish TaskFailed
//!
//! Panic:
//!   task panics ─► caught ─► treated as Err(Fail) ─► publish TaskFailed
//!
//! Timeout:
//!   timeout exceeded ─► cancel child ─► publish TimeoutHit
//!                                    ─► return Timeout error
//!                                    ─► publish TaskFailed (timeout)
//! ```
//!
//! ## Rules
//!
//! - `TimeoutHit` is an **informational** event published **before** `TaskFailed` on timeout *(it is not a terminal event itself)*
//! - `Canceled` is treated as graceful exit → `TaskCanceled` (not `TaskFailed`).
//! - A **panic** in the task body (or in `spawn()` itself) is caught and converted to a retryable [`TaskError::Fail`] with reason `task panicked: ...` - restart policy applies.
//! - Always publishes **exactly one** terminal event per attempt: `TaskStopped`, `TaskCanceled`, or `TaskFailed`.
//! - Derives **child token** per attempt (isolated cancellation).
//! - Child cancellation does **not** affect parent.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::time;
use tokio_util::sync::CancellationToken;

use crate::{
    error::TaskError,
    events::{Bus, Event, EventKind},
    identity::TaskId,
    tasks::{BoxTaskFuture, Task, TaskContext},
};

/// Future adapter that traps panics raised by the task body.
///
/// Polls the inner future inside [`std::panic::catch_unwind`], converting a panic into a retryable [`TaskError::Fail`];
/// actor applies the normal restart/backoff policy instead of unwinding (which would leave the task unreaped in the registry and never restarted).
struct CatchPanic(BoxTaskFuture);

impl Future for CatchPanic {
    type Output = Result<(), TaskError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        std::panic::catch_unwind(AssertUnwindSafe(|| self.0.as_mut().poll(cx)))
            .unwrap_or_else(|payload| Poll::Ready(Err(panic_to_error(payload.as_ref()))))
    }
}

/// Maps a panic payload to a retryable [`TaskError::Fail`] with the panic message.
fn panic_to_error(payload: &(dyn std::any::Any + Send)) -> TaskError {
    let msg = payload
        .downcast_ref::<&'static str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload");
    TaskError::Fail {
        reason: format!("task panicked: {msg}"),
        exit_code: None,
    }
}

/// Executes a single attempt of `task`, publishing lifecycle events to `bus`.
///
/// # Also
///
/// - [`TaskActor`](super::actor::TaskActor) - calls `run_once` in a retry loop
/// - [`Event`](crate::Event) / [`EventKind`](crate::EventKind) - published lifecycle events
///
/// ### Flow
///
/// 1. Derive child cancellation token from parent
/// 2. Execute task with optional timeout wrapper
/// 3. Publish terminal event based on result
///
/// ### Timeout behavior
///
/// If `timeout` is `Some(dur)` and `dur > 0`:
/// - Wraps execution in `tokio::time::timeout`
/// - On timeout: cancels child token, publishes `TimeoutHit`, returns `Timeout` error
///
/// ### Cancellation semantics
///
/// - Parent cancellation propagates to child token
/// - Task **should** return `Err(TaskError::Canceled)` when detecting cancellation
/// - `Canceled` is treated as **graceful exit**, publishes `TaskCanceled` (not `TaskFailed`)
/// - Child cancellation does **not** affect parent (isolated per attempt)
///
/// ### Event semantics
///
/// Always publishes **exactly one** terminal event per attempt:
/// - `TaskStopped` on `Ok(())`, `TaskCanceled` on `Err(Canceled)` (graceful exits)
/// - `TaskFailed` on `Err(Fail/Fatal/Timeout)` (actual errors)
///
/// `TimeoutHit` is informational and published **before** `TaskFailed` on timeout.
pub async fn run_once<T: Task + ?Sized>(
    task: &T,
    parent: &CancellationToken,
    timeout: Option<Duration>,
    attempt: u32,
    id: TaskId,
    bus: &Bus,
) -> Result<(), TaskError> {
    let child = parent.child_token();
    let ctx = TaskContext::from_token(child.clone());

    let fut = match std::panic::catch_unwind(AssertUnwindSafe(move || task.spawn(ctx))) {
        Ok(fut) => CatchPanic(fut),
        Err(payload) => {
            let e = panic_to_error(payload.as_ref());
            publish_failed(bus, id, task.name(), attempt, &e);
            return Err(e);
        }
    };

    let res = if let Some(dur) = timeout.filter(|d| *d > Duration::ZERO) {
        match time::timeout(dur, fut).await {
            Ok(r) => r,
            Err(_elapsed) => {
                child.cancel();
                publish_timeout(bus, id, task.name(), dur, attempt);
                Err(TaskError::Timeout { timeout: dur })
            }
        }
    } else {
        fut.await
    };

    match res {
        Ok(()) => {
            publish_stopped(bus, id, task.name(), attempt);
            Ok(())
        }
        Err(TaskError::Canceled) => {
            publish_canceled(bus, id, task.name(), attempt);
            Err(TaskError::Canceled)
        }
        Err(e) => {
            publish_failed(bus, id, task.name(), attempt, &e);
            Err(e)
        }
    }
}

/// Publishes `TaskStopped` event.
///
/// successful attempt.
fn publish_stopped(bus: &Bus, id: TaskId, name: &str, attempt: u32) {
    bus.publish(
        Event::new(EventKind::TaskStopped)
            .with_task(name)
            .with_id(id)
            .with_attempt(attempt),
    );
}

/// Publishes `TaskCanceled` event.
///
/// graceful cancellation of an attempt.
fn publish_canceled(bus: &Bus, id: TaskId, name: &str, attempt: u32) {
    bus.publish(
        Event::new(EventKind::TaskCanceled)
            .with_task(name)
            .with_id(id)
            .with_attempt(attempt),
    );
}

/// Publishes `TaskFailed` event with error details.
fn publish_failed(bus: &Bus, id: TaskId, name: &str, attempt: u32, err: &TaskError) {
    let mut ev = Event::new(EventKind::TaskFailed)
        .with_task(name)
        .with_id(id)
        .with_attempt(attempt)
        .with_reason(err.to_string());
    if let Some(code) = err.exit_code() {
        ev = ev.with_exit_code(code);
    }
    bus.publish(ev);
}

/// Publishes `TimeoutHit` event.
///
/// always followed by `TaskFailed`.
fn publish_timeout(bus: &Bus, id: TaskId, name: &str, dur: Duration, attempt: u32) {
    bus.publish(
        Event::new(EventKind::TimeoutHit)
            .with_task(name)
            .with_id(id)
            .with_timeout(dur)
            .with_attempt(attempt),
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

    struct OkTask;

    impl Task for OkTask {
        fn name(&self) -> &str {
            "ok-task"
        }

        fn spawn(&self, _ctx: TaskContext) -> BoxFut {
            Box::pin(async { Ok(()) })
        }
    }

    struct FailTask;

    impl Task for FailTask {
        fn name(&self) -> &str {
            "fail-task"
        }

        fn spawn(&self, _ctx: TaskContext) -> BoxFut {
            Box::pin(async {
                Err(TaskError::Fail {
                    reason: "boom".into(),
                    exit_code: None,
                })
            })
        }
    }

    #[tokio::test]
    async fn test_timeout_returns_timeout_variant_not_fail() {
        let bus = Bus::new(16);
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
    }

    #[tokio::test]
    async fn test_success_publishes_stopped_with_attempt() {
        let bus = Bus::new(16);
        let mut rx = bus.subscribe();
        let parent = CancellationToken::new();

        let _ = run_once(&OkTask, &parent, None, 3, TaskId::next(), &bus).await;

        let mut stopped_attempt = None;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev.kind, EventKind::TaskStopped) {
                stopped_attempt = ev.attempt;
            }
        }
        assert_eq!(
            stopped_attempt,
            Some(3),
            "TaskStopped must carry the attempt number"
        );
    }

    #[tokio::test]
    async fn test_success_returns_ok() {
        let bus = Bus::new(16);
        let parent = CancellationToken::new();

        let result = run_once(&OkTask, &parent, None, 1, TaskId::next(), &bus).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_failure_returns_fail_variant() {
        let bus = Bus::new(16);
        let parent = CancellationToken::new();

        let result = run_once(&FailTask, &parent, None, 1, TaskId::next(), &bus).await;

        assert!(
            matches!(result, Err(TaskError::Fail { .. })),
            "expected TaskError::Fail, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_timeout_publishes_timeout_hit_event() {
        let bus = Bus::new(16);
        let mut rx = bus.subscribe();
        let parent = CancellationToken::new();

        let _ = run_once(
            &SlowTask,
            &parent,
            Some(Duration::from_millis(50)),
            1,
            TaskId::next(),
            &bus,
        )
        .await;

        let mut saw_timeout_hit = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev.kind, EventKind::TimeoutHit) {
                saw_timeout_hit = true;
            }
        }
        assert!(saw_timeout_hit, "expected TimeoutHit event to be published");
    }
}
