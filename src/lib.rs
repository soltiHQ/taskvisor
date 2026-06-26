//! # taskvisor
//!
//! **Taskvisor** is a lightweight task orchestration library for Rust.
//!
//! It provides primitives to define, supervise, and restart async tasks with configurable policies.
//! The crate is designed as a building block for higher-level orchestrators and agents.
//!
//! ## Architecture
//! ### Overview
//!
//! ```text
//!     ┌──────────────┐   ┌──────────────┐   ┌──────────────┐
//!     │   TaskSpec   │   │   TaskSpec   │   │   TaskSpec   │
//!     │(user task #1)│   │(user task #2)│   │(user task #3)│
//!     └──────┬───────┘   └──────┬───────┘   └──────┬───────┘
//!            ▼                  ▼                  ▼
//! ┌───────────────────────────────────────────────────────────────────┐
//! │  Supervisor (runtime orchestrator)                                │
//! │  - Bus (broadcast events)                                         │
//! │  - AliveTracker (tracks task state with sequence numbers)         │
//! │  - SubscriberSet (fans out to user subscribers)                   │
//! │  - Registry (manages active tasks by name)                        │
//! └──────┬──────────────────┬──────────────────┬───────────────┬──────┘
//!        ▼                  ▼                  ▼               │
//!     ┌──────────────┐   ┌──────────────┐   ┌──────────────┐   │
//!     │  TaskActor   │   │  TaskActor   │   │  TaskActor   │   │
//!     │ (retry loop) │   │ (retry loop) │   │ (retry loop) │   │
//!     └┬─────────────┘   └┬─────────────┘   └┬─────────────┘   │
//!      │                  │                  │                 │
//!      │ Publishes        │ Publishes        │ Publishes       │
//!      │ Events:          │ Events:          │ Events:         │
//!      │ - TaskStarting   │ - TaskStarting   │ - TaskStarting  │
//!      │ - TaskFailed     │ - TaskStopped    │ - TimeoutHit    │
//!      │ - BackoffSched.  │ - ActorExhausted │ - ...           │
//!      │                  │                  │                 │
//!      ▼                  ▼                  ▼                 ▼
//! ┌───────────────────────────────────────────────────────────────────┐
//! │                        Bus (broadcast channel)                    │
//! │              (capacity: SupervisorConfig::bus_capacity)           │
//! └─────────────────────────────────┬─────────────────────────────────┘
//!                                   ▼
//!                       ┌────────────────────────┐
//!                       │  subscriber_listener   │
//!                       │   (in Supervisor)      │
//!                       └───┬────────────────┬───┘
//!                           ▼                ▼
//!                    AliveTracker     SubscriberSet
//!                  (sequence-based)   (per-sub queues)
//!                                  ┌─────────┼─────────┐
//!                                  ▼         ▼         ▼
//!                                  worker1  worker2  workerN
//!                                  ▼         ▼         ▼
//!                             sub1.on   sub2.on   subN.on
//!                              _event()  _event()  _event()
//! ```
//!
//! ### Lifecycle
//!
//! ```text
//! TaskSpec ──► Supervisor ──► Registry ──► TaskActor::run()
//!
//! loop {
//!   ├─► attempt += 1
//!   ├─► acquire semaphore (optional, cancellable)
//!   ├─► publish TaskStarting{ task, attempt }
//!   ├─► run_once(task, timeout, attempt)
//!   │       │
//!   │       ├─ Ok  ──► publish TaskStopped
//!   │       │          ├─ RestartPolicy::Never     ─► ActorExhausted, exit
//!   │       │          ├─ RestartPolicy::OnFailure ─► ActorExhausted, exit
//!   │       │          └─ RestartPolicy::Always    ─► reset delay, continue
//!   │       │
//!   │       └─ Err ──► publish TaskFailed{ task, error, attempt }
//!   │                  ├─ RestartPolicy::Never     ─► ActorExhausted, exit
//!   │                  └─ RestartPolicy::OnFailure/Always:
//!   │                       ├─ compute delay = backoff.next(backoff_attempt)
//!   │                       ├─ publish BackoffScheduled{ delay, attempt }
//!   │                       ├─ sleep(delay) (cancellable)
//!   │                       └─ continue
//!   │
//!   └─ exit conditions:
//!        - runtime_token cancelled (OS signal or explicit remove)
//!        - RestartPolicy forbids continuation ─► ActorExhausted
//!        - Fatal error ─► ActorDead
//!        - semaphore closed
//! }
//!
//! On exit: actor cleanup removes from Registry (if PolicyExhausted/Fatal)
//! ```
//!
//! ## Features
//!
//! | Area              | Description                                                            | Key types / traits                     |
//! |-------------------|------------------------------------------------------------------------|----------------------------------------|
//! | **Subscriber API**| Hook into task lifecycle events (logging, metrics, custom subscribers).| [`Subscribe`]                          |
//! | **Policies**      | Configure restart/backoff strategies for tasks.                        | [`RestartPolicy`], [`BackoffPolicy`]   |
//! | **Supervision**   | Manage groups of tasks and their lifecycle.                            | [`Supervisor`], [`SupervisorHandle`]   |
//! | **Completion**    | Opt in (via `*_and_watch`) to await a task's final result.             | [`TaskWaiter`], [`TaskOutcome`]        |
//! | **Errors**        | Typed errors for orchestration and task execution.                     | [`TaskError`], [`RuntimeError`]        |
//! | **Tasks**         | Define tasks as functions or specs, easy to compose and run.           | [`TaskRef`], [`TaskFn`], [`TaskSpec`]  |
//! | **Configuration** | Centralize runtime settings.                                           | [`SupervisorConfig`]                   |
//!
//! ## Optional features
//!
//! - `logging`: exports a simple built-in `LogWriter` _(demo/reference only)_.
//! - `controller`:  exposes controller runtime and admission types.
//!
//! ## Example
//!
//! ```rust
//! use taskvisor::prelude::*;
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let sup = Supervisor::new(SupervisorConfig::default(), vec![]);
//!
//!     // Define a simple task that runs once and exits
//!     let hello: TaskRef = TaskFn::arc("hello", |ctx: TaskContext| async move {
//!         if ctx.is_cancelled() { return Ok(()); }
//!         println!("Hello from task!");
//!         Ok(())
//!     });
//!
//!     // One-shot task (runs once, never restarts)
//!     let spec = TaskSpec::once(hello);
//!
//!     sup.run(vec![spec]).await?;
//!     Ok(())
//! }
//! ```

#![forbid(unsafe_code)]

/// Compiles the runnable Rust code blocks in `README.md` as doctests
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

pub mod prelude;

mod identity;
pub use identity::TaskId;

mod core;
pub use core::{
    Supervisor, SupervisorBuilder, SupervisorConfig, SupervisorHandle, TaskOutcome, TaskWaiter,
};

mod tasks;
pub use tasks::{BoxTaskFuture, Task, TaskContext, TaskFn, TaskRef, TaskSpec};

mod policies;
pub use policies::{BackoffPolicy, JitterPolicy, RestartPolicy};

mod events;
pub use events::{BackoffSource, Event, EventKind};

mod error;
pub use error::{BoxError, RuntimeError, SharedError, TaskError};

mod subscribers;
pub use subscribers::Subscribe;

#[cfg(feature = "controller")]
mod controller;
#[cfg(feature = "controller")]
pub use controller::{AdmissionPolicy, ControllerConfig, ControllerError, ControllerSpec};

#[cfg(feature = "logging")]
pub use subscribers::LogWriter;
