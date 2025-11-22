//! # taskvisor
//!
//! **Taskvisor** is a lightweight task orchestration library for Rust.
//!
//! It provides primitives to define, supervise, and restart async tasks
//! with configurable policies. The crate is designed as a building block
//! for higher-level orchestrators and agents.
//!
//! ## Architecture
//! ### Overview
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
//!└─────────────────────────────────┬─────────────────────────────────┘
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
//!   │                       ├─ compute delay = backoff.next(prev_delay)
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
//! | Area              | Description                                                            | Key types / traits                     |
//! |-------------------|------------------------------------------------------------------------|----------------------------------------|
//! | **Subscriber API**| Hook into task lifecycle events (logging, metrics, custom subscribers).| [`Subscribe`]                          |
//! | **Policies**      | Configure restart/backoff strategies for tasks.                        | [`RestartPolicy`], [`BackoffPolicy`]   |
//! | **Supervision**   | Manage groups of tasks and their lifecycle.                            | [`Supervisor`]                         |
//! | **Errors**        | Typed errors for orchestration and task execution.                     | [`TaskError`], [`RuntimeError`]        |
//! | **Tasks**         | Define tasks as functions or specs, easy to compose and run.           | [`TaskRef`], [`TaskFn`], [`TaskSpec`]  |
//! | **Configuration** | Centralize runtime settings.                                           | [`SupervisorConfig`]                   |
//!
//! ## Optional features
//! - `logging`: exports a simple built-in [`LogWriter`] _(demo/reference only)_.
//! - `controller`:  exposes controller runtime and admission types.
//!
//! ## Example
//! ```rust
//! use std::sync::Arc;
//! use std::time::Duration;
//! use tokio_util::sync::CancellationToken;
//! use taskvisor::{BackoffPolicy, SupervisorConfig, RestartPolicy, Supervisor, TaskFn, TaskRef, TaskSpec};
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let mut cfg = SupervisorConfig::default();
//!     cfg.timeout = Duration::from_secs(5);
//!
//!     // Build subscribers (optional)
//!     #[cfg(feature = "logging")]
//!     let subs: Vec<Arc<dyn taskvisor::Subscribe>> = {
//!         use taskvisor::LogWriter;
//!         vec![Arc::new(LogWriter::default())]
//!     };
//!     #[cfg(not(feature = "logging"))]
//!     let subs: Vec<Arc<dyn taskvisor::Subscribe>> = Vec::new();
//!
//!     // Create supervisor
//!     let sup = taskvisor::Supervisor::builder(cfg)
//!         .with_subscribers(subs)
//!         // .with_controller(ControllerConfig { ... })  // if feature = "controller"
//!         .build();
//!
//!     // Define a simple task that runs once and exits
//!     let hello: TaskRef = TaskFn::arc("hello", |ctx: CancellationToken| async move {
//!         if ctx.is_cancelled() { return Ok(()); }
//!         println!("Hello from task!");
//!         Ok(())
//!     });
//!
//!     // Build specification - use RestartPolicy::Never for one-shot task
//!     let spec = TaskSpec::new(
//!         hello,
//!         RestartPolicy::Never,
//!         BackoffPolicy::default(),
//!         Some(Duration::from_secs(5)),
//!     );
//!
//!     // Pass initial tasks to run() - they will be added by the registry listener
//!     sup.run(vec![spec]).await?;
//!     Ok(())
//! }
//! ```
mod core;
mod error;
mod events;
mod policies;
mod subscribers;
mod tasks;

// ---- Public re-exports ----

pub use core::{Supervisor, SupervisorConfig};
pub use error::{RuntimeError, TaskError};
pub use events::{BackoffSource, Event, EventKind};
pub use policies::{BackoffPolicy, JitterPolicy, RestartPolicy};
pub use subscribers::{Subscribe, SubscriberSet};
pub use tasks::{Task, TaskFn, TaskRef, TaskSpec};

// Optional: expose a controller object.
// Enable with: `--features controller`
#[cfg(feature = "controller")]
mod controller;
#[cfg(feature = "controller")]
pub use controller::{AdmissionPolicy, ControllerConfig, ControllerError, ControllerSpec};

// Optional: expose a simple built-in logger subscriber (demo/reference).
// Enable with: `--features logging`
#[cfg(feature = "logging")]
pub use subscribers::LogWriter;
