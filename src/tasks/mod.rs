//! # Define supervised work
//!
//! This module contains the task contract and the settings for one registered task.
//!
//! | Type              | Role                                                    |
//! |-------------------|---------------------------------------------------------|
//! | [`Task`]          | Trait for user work                                     |
//! | [`TaskFn`]        | Closure-based [`Task`]                                  |
//! | [`TaskRef`]       | Shared task handle: `Arc<dyn Task>`                     |
//! | [`TaskSpec`]      | Task plus restart, backoff, timeout, and retry settings |
//! | [`BoxTaskFuture`] | Future returned by [`Task::spawn`]                      |
//!
//! ## Create a Task
//!
//! Use [`TaskFn`] for most tasks.
//! Implement [`Task`] when a named type makes dependencies or testing clearer.
//!
//! ## Data Flow
//!
//! Both paths create a [`TaskRef`].
//! A [`TaskSpec`] adds execution settings.
//! Any setting that the spec does not set comes from [`TaskDefaults`](crate::TaskDefaults) when the supervisor accepts the task.
//!
//! ```text
//! TaskFn::arc(name, closure) ─┐
//!                             ├──► TaskRef ──► TaskSpec ──► Supervisor
//! impl Task                  ─┘
//! ```
//!
//! ## Example
//!
//! ```rust
//! use taskvisor::{TaskError, TaskFn, TaskRef, TaskSpec};
//!
//! let task: TaskRef = TaskFn::arc("worker", |ctx| async move {
//!     ctx.cancelled().await;
//!     Err(TaskError::Canceled)
//! });
//!
//! // Restart after retryable failures. Stop after success or cancellation.
//! let spec = TaskSpec::restartable(task);
//! ```
//!
//! The supervisor may call the task closure more than once.
//! Each call is one attempt and must create a new future.
//! Attempts for the same registered task do not run at the same time.
//!
//! ## Attempt Safety
//!
//! - A long-running task must observe [`TaskContext`] at every wait that may last for a long time.
//! - [`TaskContext::run_until_cancelled`] drops the wrapped future when cancellation wins. Use it only with futures that are safe to cancel by dropping.
//! - An attempt timeout drops the task future.
//!   Force-abort drops it after Tokio regains control; it cannot interrupt synchronous code inside one poll.
//!   Neither path rolls back external side effects.
//! - Do not run blocking I/O or heavy CPU work directly in the task future.
//!   Move it to a suitable blocking or CPU worker.
//!   Work already running outside the async future may continue after that future is dropped.

mod task;
pub use task::{BoxTaskFuture, Task, TaskRef};

mod context;
pub use context::TaskContext;

mod r#impl;
pub use r#impl::func::TaskFn;

mod spec;
pub use spec::TaskSpec;
