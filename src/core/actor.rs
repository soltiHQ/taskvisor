//! # Task actor: runs a single task with restart/backoff/timeout semantics.
//!
//! A `TaskActor` drives one [`Task`] through repeated attempts, applying:
//! - restart policy ([`RestartPolicy`]),
//! - backoff delays ([`BackoffStrategy`]),
//! - per-attempt timeout (optional, via `timeout`),
//! - cooperative cancellation via a runtime [`CancellationToken`].
//!
//! It also publishes lifecycle [`Event`]s to the internal bus.
//!
//! # High-level architecture:
//! ```text
//! TaskSpec ──► Supervisor ──► TaskActor (from TaskSpec)
//!
//! attempt ──► run_once(task, timeout)
//!      │       ├── Ok  ─► apply RestartPolicy(Never/OnFailure/Always)
//!      │       └── Err ─► schedule backoff ─► sleep ─► retry
//!      └────► cancelled (runtime_token) ─► exit
//! ```
//!
//! #### Notes:
//! - One TaskActor runs attempts strictly sequentially (never in parallel).
//! - Parallelism is possible only if you: **spawn multiple actors for the same Task**

use std::sync::Arc;
use std::time::Duration;

use tokio::{select, sync::Semaphore, time};
use tokio_util::sync::CancellationToken;

use crate::core::runner::run_once;
use crate::event::strategy::BackoffStrategy;
use crate::{
    event::Bus,
    event::{Event, EventKind},
    policy::RestartPolicy,
    task::Task,
};

/// Parameters controlling retries/backoff/timeout for a task actor.
#[derive(Clone)]
pub struct TaskActorParams {
    /// Restart policy applied after each attempt.
    pub restart: RestartPolicy,
    /// Backoff strategy used between failed attempts.
    pub backoff: BackoffStrategy,
    /// Optional per-attempt timeout; `None` or `0` means no timeout.
    pub timeout: Option<Duration>,
}

/// Drives a single [`Task`] with retries and backoff, publishing events.
///
/// The actor:
/// - acquires an optional global semaphore permit (if provided),
/// - emits [`EventKind::TaskStarting`] with the attempt number,
/// - calls [`run_once`] to execute the task with optional timeout,
/// - on success applies the restart policy,
/// - on failure decides whether to retry and, if so, schedules backoff
///   ([`EventKind::BackoffScheduled`]) and sleeps unless cancelled.
/// - exits promptly when the runtime token is cancelled.
pub struct TaskActor {
    /// Task to execute.
    pub task: Arc<dyn Task>,
    /// Retry/backoff/timeout parameters.
    pub params: TaskActorParams,
    /// Internal event bus (used to publish lifecycle events).
    pub bus: Bus,
    /// Optional global concurrency limiter.
    pub global_sem: Option<Arc<Semaphore>>,
}

impl TaskActor {
    /// Creates a new task actor with the given task, params, bus and semaphore.
    pub fn new(
        task: Arc<dyn Task>,
        params: TaskActorParams,
        bus: Bus,
        global_sem: Option<Arc<Semaphore>>,
    ) -> Self {
        Self {
            task,
            params,
            bus,
            global_sem,
        }
    }

    /// Runs the actor until completion, restart exhaustion, or cancellation.
    ///
    /// Cancellation semantics:
    /// - The provided `runtime_token` is checked between phases (permits, backoff sleep).
    /// - On timeout inside [`run_once`], the child token is cancelled.
    ///
    /// Concurrency semantics:
    /// - If `global_sem` is present, a permit is acquired per attempt.
    ///   Acquisition is cancellable via `runtime_token`.
    pub async fn run(self, runtime_token: CancellationToken) {
        let mut attempt: u64 = 0;
        let mut prev_delay: Option<Duration> = None;

        loop {
            if runtime_token.is_cancelled() {
                break;
            }
            let _permit_guard = match &self.global_sem {
                Some(sem) => {
                    let permit_fut = sem.clone().acquire_owned();

                    tokio::pin!(permit_fut);
                    select! {
                        res = &mut permit_fut => { res.ok() }
                        _ = runtime_token.cancelled() => { break; }
                    }
                }
                None => None,
            };

            attempt += 1;
            self.bus.publish(
                Event::now(EventKind::TaskStarting)
                    .with_task(self.task.name())
                    .with_attempt(attempt),
            );

            let res = run_once(
                self.task.as_ref(),
                &runtime_token,
                self.params.timeout,
                &self.bus,
            )
            .await;

            match res {
                Ok(()) => {
                    prev_delay = None;
                    match self.params.restart {
                        RestartPolicy::Never => break,
                        RestartPolicy::OnFailure => break,
                        RestartPolicy::Always => continue,
                    }
                }
                Err(e) => {
                    let should_retry = matches!(
                        self.params.restart,
                        RestartPolicy::OnFailure | RestartPolicy::Always
                    );
                    if !should_retry {
                        break;
                    }

                    let delay = self.params.backoff.next(prev_delay);
                    prev_delay = Some(delay);

                    self.bus.publish(
                        Event::now(EventKind::BackoffScheduled)
                            .with_task(self.task.name())
                            .with_delay(delay)
                            .with_attempt(attempt)
                            .with_error(e.to_string()),
                    );

                    let sleep = time::sleep(delay);
                    tokio::pin!(sleep);
                    select! {
                        _ = &mut sleep => {}
                        _ = runtime_token.cancelled() => { break; }
                    }
                }
            }
        }
    }
}
