//! # Task abstractions.
//!
//! This module contains the types used to define work and describe how it runs.
//!
//! | Type              | Role                                                |
//! |-------------------|-----------------------------------------------------|
//! | [`Task`]          | Trait for user work                                 |
//! | [`TaskFn`]        | Closure-based [`Task`]                              |
//! | [`TaskRef`]       | Shared task handle: `Arc<dyn Task>`                 |
//! | [`TaskSpec`]      | Run config: restart, backoff, timeout, retry limit  |
//! | [`BoxTaskFuture`] | Future returned by [`Task::spawn`]                  |
//!
//! ## Creating A Task
//!
//! - Use [`TaskFn`] for most tasks. Pass a name and an async closure.
//! - Use `impl Task` when the task needs its own type or shared dependencies.
//!
//! ## Data Flow
//!
//! Both paths create a [`TaskRef`]. Wrap it in a [`TaskSpec`] to choose restart policy, backoff, timeout, and retry limit.
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
//! let task: TaskRef = TaskFn::arc("worker", |_ctx: TaskContext| async move {
//!     Ok::<(), TaskError>(())
//! });
//!
//! let once = TaskSpec::once(task.clone());
//! let restartable = TaskSpec::restartable(task.clone());
//! ```
//!
//! Use [`SupervisorConfig::task_spec`](crate::SupervisorConfig::task_spec) when you want a task to inherit supervisor defaults.

mod task;
pub use task::{BoxTaskFuture, Task, TaskRef};

mod context;
pub use context::TaskContext;

mod r#impl;
pub use r#impl::func::TaskFn;

mod spec;
pub use spec::TaskSpec;
