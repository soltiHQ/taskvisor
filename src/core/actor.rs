//! # TaskActor: single-task supervisor.
//!
//! Supervises execution of one [`Task`] with policies:
//! - restarts per [`RestartPolicy`],
//! - delays per [`BackoffPolicy`],
//! - optional per-attempt timeout,
//! - cooperative cancellation via [`CancellationToken`].
//!
//! ## Event flow
//! For each attempt, the actor publishes:
//! ```text
//! TaskStarting → [task execution] → TaskStopped (success)
//!                                 → TimeoutHit (timeout)
//!                                 → TaskFailed (error)
//!
//! If retry scheduled:
//!   → BackoffScheduled → [sleep] → (next attempt with new seq)
//! ```
//!
//! ## Architecture
//! ```text
//! TaskSpec ──► Supervisor ──► TaskActor::run()
//!
//! loop {
//!   ├─► acquire semaphore
//!   ├─► publish TaskStarting
//!   ├─► run_once() ─────► task.spawn()
//!   │       │                  ▼
//!   │       │            (one attempt)
//!   │       ▼                  ▼
//!   │     Ok/Err ──► publish TaskStopped/TaskFailed
//!   │       ▼
//!   ├─► apply RestartPolicy
//!   │     ├─► Never       → break
//!   │     ├─► OnFailure   → break if Ok
//!   │     └─► Always      → continue
//!   └─► if retry:
//!        ├─► publish BackoffScheduled
//!        └─► sleep(backoff_delay)
//! }
//! ```
//!
//! ## Rules
//! - Attempts run **sequentially** within one actor (never parallel)
//! - Attempt counter **increments on each spawn** (monotonic, never resets)
//! - Events have **monotonic sequence numbers** (ordering guarantees)

use std::{sync::Arc, time::Duration};

use tokio::{select, sync::Semaphore, time};
use tokio_util::sync::CancellationToken;

use crate::{
    core::runner::run_once,
    events::{Bus, Event, EventKind},
    policies::{BackoffPolicy, RestartPolicy},
    tasks::Task,
};

/// Configuration parameters for a task actor.
///
/// These parameters are extracted from a [`TaskSpec`](crate::TaskSpec)
/// by the [`Supervisor`](crate::Supervisor) when spawning actors.
#[derive(Clone)]
pub struct TaskActorParams {
    /// When to restart the task.
    pub restart: RestartPolicy,
    /// How to compute retry delays.
    pub backoff: BackoffPolicy,
    /// Optional per-attempt timeout (`None` = no timeout).
    pub timeout: Option<Duration>,
}

/// Supervises execution of a single [`Task`] with retries, backoff, and event publishing.
///
/// ### Responsibilities
/// - **Concurrency control**: Acquires semaphore permit before each attempt
/// - **Graceful shutdown**: Responds to cancellation at safe points
/// - **Event publishing**: Reports all lifecycle events to the bus
/// - **Execution**: Runs the task via [`run_once`]
/// - **Restart policy**: Supervises by the [`TaskActorParams`](crate::TaskActorParams)
///
/// ### Rules
/// - Attempts run **sequentially** (never concurrent for one actor)
/// - Cancellation is checked at **safe points** (semaphore acquire, backoff sleep)
/// - Events are published with **monotonic sequence numbers** (ordering)
/// - Backoff counter **resets on success** (healthy system assumption)
pub struct TaskActor {
    /// Task to execute.
    pub task: Arc<dyn Task>,
    /// Parameters for supervise task executions.
    pub params: TaskActorParams,
    /// Internal event bus (used to publish lifecycle events).
    pub bus: Bus,
    /// Optional global tasks concurrency limiter.
    pub semaphore: Option<Arc<Semaphore>>,
}

impl TaskActor {
    /// Creates a new task actor.
    pub fn new(
        bus: Bus,
        task: Arc<dyn Task>,
        params: TaskActorParams,
        semaphore: Option<Arc<Semaphore>>,
    ) -> Self {
        Self {
            task,
            params,
            bus,
            semaphore,
        }
    }

    /// Runs the actor until completion, restart exhaustion, or cancellation.
    ///
    /// This is the main actor loop. It will:
    /// 1. Acquire semaphore permit (if configured)
    /// 2. Publish `TaskStarting` event
    /// 3. Execute one attempt via `run_once`
    /// 4. Apply restart policy (defined in params)
    /// 5. If retry needed, publish `BackoffScheduled` and sleep
    /// 6. Repeat until exit condition
    ///
    /// ### Exit conditions
    /// The actor stops when:
    /// - Task succeeds and restart policy forbids continuation
    /// - Task fails and restart policy forbids retry
    /// - `runtime_token` is cancelled (shutdown signal)
    /// - Semaphore is closed (runtime shutdown in progress)
    ///
    /// ### Cancellation semantics
    /// - `runtime_token` is checked at **safe points** only:
    ///   - Before semaphore acquisition
    ///   - During semaphore acquisition (cancellable wait)
    ///   - During backoff sleep (cancellable wait)
    /// - Task execution receives a **child token** that gets cancelled on timeout
    /// - Cancellation during backoff **aborts sleep** immediately
    ///
    /// ### Backoff semantics
    /// - First retry uses `BackoffPolicy::first` delay
    /// - Subsequent retries multiply previous delay by `factor`
    /// - Delays are capped at `BackoffPolicy::max`
    /// - Jitter is applied according to `BackoffPolicy::jitter`
    /// - Attempt counter **never resets** (monotonic lifetime counter)
    ///
    /// ### Observability
    /// All lifecycle events are published to the bus for subscribers to process:
    /// - `TaskStarting`: attempt started (includes attempt number)
    /// - `TaskStopped`: attempt succeeded
    /// - `TaskFailed`: attempt failed (includes error)
    /// - `TimeoutHit`: attempt timed out
    /// - `BackoffScheduled`: retry scheduled (includes delay and attempt number)
    ///
    /// Subscribers can implement metrics, logging, alerting, etc.
    pub async fn run(self, runtime_token: CancellationToken) {
        let mut prev_delay: Option<Duration> = None;
        let mut attempt: u64 = 0;

        loop {
            if runtime_token.is_cancelled() {
                break;
            }
            let _guard = match &self.semaphore {
                Some(sem) => {
                    let permit_future = sem.clone().acquire_owned();
                    tokio::pin!(permit_future);

                    select! {
                        res = &mut permit_future => {
                            match res {
                                Ok(permit) => Some(permit),
                                Err(_closed) => { break; }
                            }
                        }
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
                attempt,
                &self.bus,
            )
            .await;

            match res {
                Ok(()) => {
                    prev_delay = None;

                    match self.params.restart {
                        RestartPolicy::Always => continue,
                        RestartPolicy::OnFailure => break,
                        RestartPolicy::Never => break,
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
