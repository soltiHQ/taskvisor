//! # taskvisor
//!
//! **Taskvisor** is a lightweight task orchestration library.
//!
//! It provides primitives to define, supervise, and restart async tasks
//! with configurable policies. The crate is designed as a building block
//! for higher-level orchestrators and agents.
//!
//! ## Features
//!
//! | Area              | Description                                                           | Key types / traits                     |
//! |-------------------|-----------------------------------------------------------------------|----------------------------------------|
//! | **Observer API**  | Hook into task lifecycle events (logging, metrics, custom observers). | [`Observer`]                           |
//! | **Policies**      | Configure restart/backoff strategies for tasks.                       | [`RestartPolicy`], [`BackoffStrategy`] |
//! | **Supervision**   | Manage groups of tasks and their lifecycle.                           | [`Supervisor`]                         |
//! | **Errors**        | Typed errors for orchestration and task execution.                    | [`TaskError`], [`RuntimeError`]        |
//! | **Tasks**         | Define tasks as functions or specs, easy to compose and run.          | [`TaskRef`], [`TaskFn`], [`TaskSpec`]  |
//! | **Configuration** | Centralize runtime settings.                                          | [`Config`]                             |
//!
//! ## Optional features
//! - `logging`: exports a simple built-in [`LoggerObserver`] _(demo/reference only)_.
//! - `events`: exports [`Event`] and [`EventKind`] for advanced integrations.
//!
//! ```no_run
//! use std::time::Duration;
//! use tokio_util::sync::CancellationToken;
//! use taskvisor::{
//!     BackoffStrategy, Config, RestartPolicy, Supervisor, TaskFn, TaskRef, TaskSpec, LoggerObserver
//! };
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let mut cfg = Config::default();
//!     cfg.timeout = Duration::from_secs(5);
//!
//!     // Use the built-in logger observer (enabled via --features "logging").
//!     let mut s = Supervisor::new(cfg.clone(), LoggerObserver);
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
//!         BackoffStrategy::default(),
//!         Some(Duration::from_secs(5)),
//!     );
//!
//!     s.run(vec![spec]).await?;
//!     Ok(())
//! }
//! ```
//!
//! ---

mod actor;
mod alive;
mod bus;
mod config;
mod error;
mod event;
mod observer;
mod policy;
mod runner;
mod strategy;
mod supervisor;
mod task;
mod task_spec;

// ---- Public re-exports ----

pub use config::Config;
pub use error::{RuntimeError, TaskError};
pub use observer::Observer;
pub use policy::RestartPolicy;
pub use strategy::BackoffStrategy;
pub use supervisor::Supervisor;
pub use task::{Task, TaskFn, TaskRef};
pub use task_spec::TaskSpec;

// Optional: expose event types.
// Enable with: `--features events`
#[cfg(feature = "events")]
pub use crate::event::{Event, EventKind};

// Optional: expose a simple built-in logger observer (demo/reference).
// Enable with: `--features logging`
#[cfg(feature = "logging")]
pub use observer::LoggerObserver;
