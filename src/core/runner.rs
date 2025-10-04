//! # Run a single attempt of a task execution.
//!
//! Executes one attempt of a [`Task`] with optional timeout, publishes lifecycle Task events to [`Bus`].
//! - **This module**: executes ONE attempt, publishes success/failure events
//! - **Task trait**: implements actual work logic, checks cancellation
//!
//! ## Event flow
//!
//! ```text
//! Success:
//!   task.spawn() → Ok  → publish TaskStopped
//!
//! Failure:
//!   task.spawn() → Err → publish TaskFailed
//!
//! Timeout:
//!   timeout → cancel child → publish TimeoutHit
//!                          → publish TaskFailed (with timeout error)
//! ```
//!
//! ## Rules
//! - Always publishes **exactly one** terminal event: `TaskStopped` or `TaskFailed`
//! - `TimeoutHit` is published **in addition to** `TaskFailed` on timeout
//! - Derives **child token** per attempt (isolated cancellation)
//! - Timeout cancels **child token** only (parent unaffected)

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
/// 1. Derive child cancellation token from parent
/// 2. Execute task with optional timeout wrapper
/// 3. Publish terminal event (`TaskStopped` or `TaskFailed`)
///
/// ### Timeout behavior
/// If `timeout` is `Some(dur)` and `dur > 0`:
/// - Wraps execution in `tokio::time::timeout`
/// - On timeout: cancels child token, publishes `TimeoutHit`, then `TaskFailed`
///
/// ### Cancellation semantics
/// - Parent cancellation propagates to child token
/// - Task **must** check `child.is_cancelled()` periodically to exit promptly
/// - Child cancellation does **not** affect parent (isolated per attempt)
///
/// ### Event semantics
/// Always publishes **exactly one** terminal event:
/// - `TaskStopped` on success
/// - `TaskFailed` on error (including `TimeoutHit` if timeout exist)
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
                publish_timeout(bus, task.name(), dur);
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
        Err(e) => {
            publish_failed(bus, task.name(), attempt, &e);
            Err(e)
        }
    }
}

/// Publishes `TaskStopped` event.
fn publish_stopped(bus: &Bus, name: &str) {
    bus.publish(Event::now(EventKind::TaskStopped).with_task(name));
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
fn publish_timeout(bus: &Bus, name: &str, dur: Duration) {
    bus.publish(
        Event::now(EventKind::TimeoutHit)
            .with_task(name)
            .with_timeout(dur),
    );
}
