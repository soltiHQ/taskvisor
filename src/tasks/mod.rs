//! # Task abstractions.
//!
//! This module contains the types used to define work and describe how it runs.
//!
//! | Type              | Role                                                |
//! |-------------------|-----------------------------------------------------|
//! | [`Task`]          | Trait for user work                                 |
//! | [`TaskFn`]        | Closure-based [`Task`]                              |
//! | [`TaskRef`]       | Shared task handle: `Arc<dyn Task>`                 |
//! | [`TaskSpec`]      | Explicit and inherited task execution settings     |
//! | [`BoxTaskFuture`] | Future returned by [`Task::spawn`]                  |
//!
//! ## Creating A Task
//!
//! - Use [`TaskFn`] for most tasks. Pass a name and an async closure.
//! - Use `impl Task` when the task needs its own type or shared dependencies.
//!
//! ## Data Flow
//!
//! Both paths create a [`TaskRef`]. Wrap it in a [`TaskSpec`] to choose a restart
//! policy and override any settings inherited from [`TaskDefaults`](crate::TaskDefaults).
//!
//! ```text
//! TaskFn::arc(name, closure) ─┐
//!                             ├──► TaskRef ──► TaskSpec ──► Supervisor
//! impl Task                  ─┘
//! ```
//!
//! ## Quick Start
//!
//! ```rust
//! use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef, TaskSpec};
//!
//! let task: TaskRef = TaskFn::arc("worker", |_ctx| async move {
//!     Ok(())
//! });
//!
//! let once = TaskSpec::once(task.clone());
//! let restartable = TaskSpec::restartable(task.clone());
//! let inherited = TaskSpec::from_defaults(task);
//! ```
//!
//! Named `TaskSpec` constructors inherit the settings they do not set. The
//! supervisor applies its [`TaskDefaults`](crate::TaskDefaults) when it admits a task.

mod task;
pub use task::{BoxTaskFuture, Task, TaskRef};

mod context;
pub use context::TaskContext;

mod r#impl;
pub use r#impl::func::TaskFn;

mod spec;
pub use spec::TaskSpec;
