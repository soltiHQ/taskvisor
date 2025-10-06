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
    attempt: u64,
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
            publish_stopped(bus, task.name(), attempt);
            Ok(())
        }
        Err(TaskError::Canceled) => {
            publish_stopped(bus, task.name(), attempt);
            Err(TaskError::Canceled)
        }
        Err(e) => {
            publish_failed(bus, task.name(), attempt, &e);
            Err(e)
        }
    }
}

/// Publishes `TaskStopped` event (success or graceful cancellation).
fn publish_stopped(bus: &Bus, name: &str, attempt: u64) {
    bus.publish(
        Event::now(EventKind::TaskStopped)
            .with_task(name)
            .with_attempt(attempt),
    );
}

/// Publishes `TaskFailed` event with error details.
fn publish_failed(bus: &Bus, name: &str, attempt: u64, err: &TaskError) {
    bus.publish(
        Event::now(EventKind::TaskFailed)
            .with_task(name)
            .with_attempt(attempt)
            .with_error(err.to_string()),
    );
}

/// Publishes `TimeoutHit` event (always followed by `TaskFailed`).
fn publish_timeout(bus: &Bus, name: &str, dur: Duration, attempt: u64) {
    bus.publish(
        Event::now(EventKind::TimeoutHit)
            .with_task(name)
            .with_timeout(dur)
            .with_attempt(attempt),
    );
}
