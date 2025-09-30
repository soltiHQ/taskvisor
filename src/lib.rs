//! # taskvisor
//!
//! **Taskvisor** is a lightweight task orchestration library.
//!
//! It provides primitives to define, supervise, and restart async tasks
//! with configurable policies. The crate is designed as a building block
//! for higher-level orchestrators and agents.
//!
//! # Architecture
//! ## High-level
//!```text
//!     ┌──────────────┐   ┌──────────────┐   ┌──────────────┐
//!     │   TaskSpec   │ … │   TaskSpec   │ … │   TaskSpec   │
//!     │(user task #1)│ … │(user task #2)│ … │(user task #3)│
//!     └──────┬───────┘   └──────┬───────┘   └──────┬───────┘
//!            ▼                  ▼                  ▼
//! ┌───────────────────────────────────────────────────────────────────┐
//! │              supervisor (create actor, handles OS signals)        │
//! └──────┬──────────────────┬──────────────────┬───────────────┬──────┘
//!        ▼                  ▼                  ▼               │
//!     ┌──────────────┐   ┌──────────────┐   ┌──────────────┐   │
//!     │  TaskActor   │   │  TaskActor   │   │  TaskActor   │   │
//!     │ (retry loop) │   │ (retry loop) │   │ (retry loop) │   │
//!     └┬─────────────┘   └┬─────────────┘   └┬─────────────┘   │
//!      │ Publishes        │ Publishes        │ Publishes       │
//!      │ Events           │ Events           │ Events          │
//!      │                  │                  │                 │
//!      │·TaskStarting     │ (…same kinds…)   │ (…same kinds…)  │
//!      │·TaskFailed       │                  │                 │
//!      │·TaskStopped      │                  │                 │
//!      │·TimeoutHit       │                  │                 │
//!      │·BackoffScheduled │                  │                 │
//!      │                  │                  │         graceful shutdown
//!      ▼                  ▼                  ▼                 ▼
//! ┌───────────────────────────────────────────────────────────────────┐
//! │                                bus                                │
//! │                          broadcast<Event>                         │
//! └─────────────────────────────────┬─────────────────────────────────┘
//!                      broadcasts to all subscribers
//!                                   ▼
//!     ┌───────────────────────┐           ┌───────────────────────┐
//!     │       Observer        │           │      AliveTracker     │
//!     │   on_event(&Event)    │           │  maintains alive set  │
//!     │    (user-defined)     │           │   (Starting/Stopped)  │
//!     └───────────────────────┘           └───────────────────────┘
//!```
//! ---
//!
//! ## Attempt flow
//!```text
//! ┌────────────────────────────────────────┐
//! │               TaskSpec                 │
//! │  {                                     │
//! │    task: TaskRef,                      │
//! │    restart: RestartPolicy,             │
//! │    backoff: BackoffPolicy,             │
//! │    timeout: Option<Duration>           │
//! │  }                                     │
//! └────┬───────────────────────────────────┘
//!      │  (constructed directly or via Config::from_task)
//!      ▼
//! ┌────────────────────────────────────────┐
//! │               TaskActor                │
//! │  { restart, backoff, timeout }         │
//! └────┬───────────────────────────────────┘
//!      │ (1) optional: acquire global Semaphore permit (cancellable)
//!      │ (2) publish Event::TaskStarting{ task, attempt }
//!      │ (3) run_once(task, attempt_timeout, bus, child_token)
//!      │         ├─ Ok  ──► publish TaskStopped
//!      │         │          └─ apply RestartPolicy from TaskSpec:
//!      │         │                - Never        ⇒ exit
//!      │         │                - OnFailure    ⇒ exit
//!      │         │                - Always       ⇒ continue
//!      │         └─ Err ──► publish TaskFailed
//!      │                    decide retry using RestartPolicy:
//!      │                      - Never        ⇒ exit
//!      │                      - OnFailure    ⇒ retry
//!      │                      - Always       ⇒ retry
//!      │ (4) if retry:
//!      │       delay = backoff.next(prev_delay) // BackoffPolicy from TaskSpec
//!      │       publish BackoffScheduled{ task, delay, attempt, error }
//!      │       sleep(delay)  (cancellable via runtime token)
//!      │       prev_delay = Some(delay)
//!      │       attempt += 1
//!      │       goto (1)
//!      └─ stop conditions:
//!              - runtime token cancelled (OS signal → graceful shutdown)
//!              - RestartPolicy (from TaskSpec) disallows further runs
//!              - semaphore closed / join end
//!```
//!---
//!
//! ## Features
//!
//! | Area              | Description                                                           | Key types / traits                     |
//! |-------------------|-----------------------------------------------------------------------|----------------------------------------|
//! | **Observer API**  | Hook into task lifecycle events (logging, metrics, custom observers). | [`Observer`]                           |
//! | **Policies**      | Configure restart/backoff strategies for tasks.                       | [`RestartPolicy`], [`BackoffPolicy`]   |
//! | **Supervision**   | Manage groups of tasks and their lifecycle.                           | [`Supervisor`]                         |
//! | **Errors**        | Typed errors for orchestration and task execution.                    | [`TaskError`], [`RuntimeError`]        |
//! | **Tasks**         | Define tasks as functions or specs, easy to compose and run.          | [`TaskRef`], [`TaskFn`], [`TaskSpec`]  |
//! | **Configuration** | Centralize runtime settings.                                          | [`Config`]                             |
//!
//! ## Optional features
//! - `logging`: exports a simple built-in [`Writer`] _(demo/reference only)_.
//! - `events`:  exports [`Event`] and [`EventKind`] for advanced integrations.
//!
//! ```no_run
//! use std::time::Duration;
//! use tokio_util::sync::CancellationToken;
//! use taskvisor::{
//!     BackoffPolicy, Config, RestartPolicy, Supervisor, TaskFn, TaskRef, TaskSpec, LogWriter
//! };
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let mut cfg = Config::default();
//!     cfg.timeout = Duration::from_secs(5);
//!
//!     // Use the built-in logger observer (enabled via --features "logging").
//!     let s = Supervisor::new(cfg.clone(), LogWriter);
//!
//!     // Define a simple task with a cancellation token.
//!     let hello: TaskRef = TaskFn::arc("hello", |ctx: CancellationToken| async move {
//!         if ctx.is_cancelled() { return Ok(()); }
//!         println!("Hello from task!");
//!         Ok(())
//!     });
//!
//!     // Build a specification for the task.
//!     let spec = TaskSpec::new(
//!         hello,
//!         RestartPolicy::Never,
//!         BackoffPolicy::default(),
//!         Some(Duration::from_secs(5)),
//!     );
//!
//!     s.run(vec![spec]).await?;
//!     Ok(())
//! }
//! ```

mod config;
mod core;
mod error;
mod events;
mod observers;
mod policy;
mod task;
// ---- Public re-exports ----

pub use config::Config;
pub use core::Supervisor;
pub use error::{RuntimeError, TaskError};
pub use observers::Observer;
pub use policy::BackoffPolicy;
pub use policy::RestartPolicy;
pub use task::{Task, TaskFn, TaskRef, TaskSpec};

// Optional: expose event types.
// Enable with: `--features events`
#[cfg(feature = "events")]
pub use crate::events::{Event, EventKind};

// Optional: expose a simple built-in logger observer (demo/reference).
// Enable with: `--features logging`
#[cfg(feature = "logging")]
pub use observers::LogWriter;
