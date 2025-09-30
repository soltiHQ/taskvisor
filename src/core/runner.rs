//! # Run a single attempt of a task with optional timeout and event reporting.
//!
//! This helper drives one execution of a [`Task`], with cancellation and publishing lifecycle [`Event`]s to the [`Bus`].
//!
//! # High-level architecture:
//! ```text
//! Task ──► run_once ──► derive child CancellationToken
//!                                    │
//!                                    ▼
//!                             ┌─────────────┐
//!       ┌─────────────────────│ timeout > 0 │
//!       │                     └─────┬───────┘
//!       no                         yes
//!       ▼                           ▼
//! task.run(child).await        tokio::time::timeout(dur, task.run(child))
//!       │                           │
//!       │                   ┌───────┴────────────────────┐
//!       │                Ok(result)                  Err(elapsed)
//!       │                   │                            │
//!    result = Ok()   → publish TaskStopped           cancel child → publish TimeoutHit
//!    result = Err(e) → publish TaskFailed
//! ```
//!
//! - On success, publishes [`EventKind::TaskStopped`].
//! - On failure, publishes [`EventKind::TaskFailed`].
//! - On timeout, cancels the child token, publishes [`EventKind::TimeoutHit`], and returns [`TaskError::Timeout`].
//! - This function performs **one attempt only**; retries/backoff are handled by [`TaskActor`](crate::core::actor::TaskActor).

use std::time::Duration;
use tokio::time;
use tokio_util::sync::CancellationToken;

use crate::{
    error::TaskError,
    events::Bus,
    events::{Event, EventKind},
    tasks::Task,
};

/// Executes a single run of a task with optional timeout.
///
/// Publishes lifecycle events to the [`Bus`] and respects cancellation tokens.
pub async fn run_once<T: Task + ?Sized>(
    task: &T,
    parent: &CancellationToken,
    timeout: Option<Duration>,
    bus: &Bus,
) -> Result<(), TaskError> {
    let child = parent.child_token();

    let res = if let Some(dur) = timeout.filter(|d| *d > Duration::ZERO) {
        match time::timeout(dur, task.run(child.clone())).await {
            Ok(r) => r,
            Err(_elapsed) => {
                child.cancel();
                publish_timeout(bus, task.name(), dur);
                return Err(TaskError::Timeout { timeout: dur });
            }
        }
    } else {
        task.run(child.clone()).await
    };

    match res {
        Ok(()) => {
            publish_stopped(bus, task.name());
            Ok(())
        }
        Err(e) => {
            publish_failed(bus, task.name(), &e);
            Err(e)
        }
    }
}

/// Publishes a `TaskStopped` event for the given task.
fn publish_stopped(bus: &Bus, name: &str) {
    bus.publish(Event::now(EventKind::TaskStopped).with_task(name));
}

/// Publishes a `TaskFailed` event with the given error.
fn publish_failed(bus: &Bus, name: &str, err: &TaskError) {
    bus.publish(
        Event::now(EventKind::TaskFailed)
            .with_task(name)
            .with_error(err.to_string()),
    );
}

/// Publishes a `TimeoutHit` event for the given task and duration.
fn publish_timeout(bus: &Bus, name: &str, dur: Duration) {
    bus.publish(
        Event::now(EventKind::TimeoutHit)
            .with_task(name)
            .with_timeout(dur),
    );
}
