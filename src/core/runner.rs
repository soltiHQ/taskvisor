//! # Run a single attempt of a task execution.
//!
//! Executes one attempt of a [`Task`] with optional timeout, publishes lifecycle events to [`Bus`].
//!
//! - **Execute ONE attempt** of the task with child cancellation token
//! - **Apply timeout** if configured (wraps execution in `tokio::time::timeout`)
//! - **Publish events** for observability (stopped/failed/timeout)
//!
//! ## Event flow
//!
//! ```text
//! Success:
//!   task.spawn() → Ok(()) → publish TaskStopped
//!
//! Cancellation:
//!   task.spawn() → Err(Canceled) → publish TaskStopped (graceful exit)
//!
//! Failure:
//!   task.spawn() → Err(Fail/Fatal) → publish TaskFailed
//!
//! Timeout:
//!   timeout exceeded → cancel child → publish TimeoutHit
//!                                   → return Timeout error
//!                                   → publish TaskFailed (timeout)
//! ```
//!
//! ## Rules
//! - Always publishes **exactly one** terminal event: `TaskStopped` or `TaskFailed`
//! - `Canceled` is treated as graceful exit → `TaskStopped` (not `TaskFailed`)
//! - `TimeoutHit` is published **in addition to** `TaskFailed` on timeout
//! - Derives **child token** per attempt (isolated cancellation)
//! - Child cancellation does **not** affect parent

use std::time::Duration;

use tokio::time;
use tokio_util::sync::CancellationToken;

use crate::{
    error::TaskError,
    events::{Bus, Event, EventKind},
    tasks::Task,
};

/// Executes a single attempt of `task`, publishing lifecycle events to `bus`.
///
/// ### Flow
/// 1. Derive child cancellation token from parent
/// 2. Execute task with optional timeout wrapper
/// 3. Publish terminal event based on result
///
/// ### Timeout behavior
/// If `timeout` is `Some(dur)` and `dur > 0`:
/// - Wraps execution in `tokio::time::timeout`
/// - On timeout: cancels child token, publishes `TimeoutHit`, returns `Timeout` error
///
/// ### Cancellation semantics
/// - Parent cancellation propagates to child token
/// - Task **should** return `Err(TaskError::Canceled)` when detecting cancellation
/// - `Canceled` is treated as **graceful exit**, publishes `TaskStopped` (not `TaskFailed`)
/// - Child cancellation does **not** affect parent (isolated per attempt)
///
/// ### Event semantics
/// Always publishes **exactly one** terminal event:
/// - `TaskStopped` on `Ok(())` or `Err(Canceled)` (graceful completion)
/// - `TaskFailed` on `Err(Fail/Fatal/Timeout)` (actual errors)
pub async fn run_once<T: Task + ?Sized>(
    task: &T,
    parent: &CancellationToken,
    timeout: Option<Duration>,
    attempt: u32,
    bus: &Bus,
) -> Result<(), TaskError> {
    let child = parent.child_token();

    let res = if let Some(dur) = timeout.filter(|d| *d > Duration::ZERO) {
        match time::timeout(dur, task.spawn(child.clone())).await {
            Ok(r) => r,
            Err(_elapsed) => {
                child.cancel();
                publish_timeout(bus, task.name(), dur, attempt);
                Err(TaskError::Timeout { timeout: dur })
            }
        }
    } else {
        task.spawn(child.clone()).await
    };

    match res {
        Ok(()) => {
            publish_stopped(bus, task.name());
            Ok(())
        }
        Err(TaskError::Canceled) => {
            publish_stopped(bus, task.name());
            Err(TaskError::Canceled)
        }
        Err(e) => {
            publish_failed(bus, task.name(), attempt, &e);
            Err(e)
        }
    }
}

/// Publishes `TaskStopped` event (success or graceful cancellation).
fn publish_stopped(bus: &Bus, name: &str) {
    bus.publish(Event::new(EventKind::TaskStopped).with_task(name));
}

/// Publishes `TaskFailed` event with error details.
fn publish_failed(bus: &Bus, name: &str, attempt: u32, err: &TaskError) {
    bus.publish(
        Event::new(EventKind::TaskFailed)
            .with_task(name)
            .with_attempt(attempt)
            .with_reason(err.to_string()),
    );
}

/// Publishes `TimeoutHit` event (always followed by `TaskFailed`).
fn publish_timeout(bus: &Bus, name: &str, dur: Duration, attempt: u32) {
    bus.publish(
        Event::new(EventKind::TimeoutHit)
            .with_task(name)
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

        fn spawn(&self, _ctx: CancellationToken) -> BoxFut {
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

        fn spawn(&self, _ctx: CancellationToken) -> BoxFut {
            Box::pin(async { Ok(()) })
        }
    }

    struct FailTask;

    impl Task for FailTask {
        fn name(&self) -> &str {
            "fail-task"
        }

        fn spawn(&self, _ctx: CancellationToken) -> BoxFut {
            Box::pin(async {
                Err(TaskError::Fail {
                    reason: "boom".into(),
                })
            })
        }
    }

    #[tokio::test]
    async fn test_timeout_returns_timeout_variant_not_fail() {
        let bus = Bus::new(16);
        let parent = CancellationToken::new();
        let timeout = Some(Duration::from_millis(50));

        let result = run_once(&SlowTask, &parent, timeout, 1, &bus).await;

        match result {
            Err(TaskError::Timeout { timeout: dur }) => {
                assert_eq!(dur, Duration::from_millis(50));
            }
            Err(TaskError::Fail { reason }) => {
                panic!("timeout should return TaskError::Timeout, not TaskError::Fail: {reason}");
            }
            other => {
                panic!("expected TaskError::Timeout, got: {other:?}");
            }
        }
    }

    #[tokio::test]
    async fn test_success_returns_ok() {
        let bus = Bus::new(16);
        let parent = CancellationToken::new();

        let result = run_once(&OkTask, &parent, None, 1, &bus).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_failure_returns_fail_variant() {
        let bus = Bus::new(16);
        let parent = CancellationToken::new();

        let result = run_once(&FailTask, &parent, None, 1, &bus).await;

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

        let _ = run_once(&SlowTask, &parent, Some(Duration::from_millis(50)), 1, &bus).await;

        let mut saw_timeout_hit = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev.kind, EventKind::TimeoutHit) {
                saw_timeout_hit = true;
            }
        }
        assert!(saw_timeout_hit, "expected TimeoutHit event to be published");
    }
}
