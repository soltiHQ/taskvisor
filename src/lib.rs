//! Public facade of the rustvisor crate.
//!
//! Public API (re-exports below) covers:
//! - Observer trait (`Observer`) so applications can hook their own logging/metrics/etc
//! - Policies / strategies (`RestartPolicy`, `BackoffStrategy`)
//! - Orchestration (`Supervisor`)
//! - Errors (`TaskError`, `RuntimeError`)
//! - Task types (`TaskRef`, `TaskFn`, `TaskSpec`)
//! - Configuration (`Config`)
//!
//! Optional API behind features:
//! - `logging`: export a built-in `LoggerObserver` (demo/reference only)
//! - `events`: export event types (`Event`, `EventKind`)

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

// Optional: expose event types (disabled by default).
// Enable with: `--features events`
#[cfg(feature = "events")]
pub use crate::event::{Event, EventKind};

// Optional: expose a simple built-in logger observer (demo/reference).
// Not enabled by default to avoid forcing a logging opinion on users.
// Enable with: `--features logging`
#[cfg(feature = "logging")]
pub use observer::LoggerObserver;
