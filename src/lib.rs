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
//! | Area              | Description                                                           | Key types / traits                 |
//! |-------------------|-----------------------------------------------------------------------|------------------------------------|
//! | **Observer API**  | Hook into task lifecycle events (logging, metrics, custom observers). | `Observer`                         |
//! | **Policies**      | Configure restart/backoff strategies for tasks.                       | `RestartPolicy`, `BackoffStrategy` |
//! | **Supervision**   | Manage groups of tasks and their lifecycle.                           | `Supervisor`                       |
//! | **Errors**        | Typed errors for orchestration and task execution.                    | `TaskError`, `RuntimeError`        |
//! | **Tasks**         | Define tasks as functions or specs, easy to compose and run.          | `TaskRef`, `TaskFn`, `TaskSpec`    |
//! | **Configuration** | Centralize runtime settings.                                          | `Config`                           |
//!
//! ## Optional features
//! - `logging`: exports a simple built-in `LoggerObserver` __(demo/reference only)__.
//! - `events`: exports `Event` and `EventKind` for advanced integrations.
//!
//! ## Example
//! ```no_run
//! use taskvisor::{Config, Supervisor, TaskFn};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let cfg = Config::default();
//!     let mut s = Supervisor::new(cfg);
//!
//!     let task = TaskFn::new("hello", |_| async {
//!         println!("Hello from task!");
//!         Ok(())
//!     });
//!
//!     s.run(vec![task.into()]).await?;
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
pub use task::{TaskFn, TaskRef};
pub use task_spec::TaskSpec;

// Optional: expose event types.
// Enable with: `--features events`
#[cfg(feature = "events")]
pub use crate::event::{Event, EventKind};

// Optional: expose a simple built-in logger observer (demo/reference).
// Enable with: `--features logging`
#[cfg(feature = "logging")]
pub use observer::LoggerObserver;
