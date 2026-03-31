//! # Task abstractions.
//!
//! | Type              | Role                                        |
//! |-------------------|---------------------------------------------|
//! | [`TaskSpec`]      | Execution config: restart, backoff, timeout |
//! | [`TaskRef`]       | Shared handle - `Arc<dyn Task>`             |
//! | [`TaskFn`]        | Closure-based [`Task`] (most common)        |
//! | [`Task`]          | **Trait**; async, cancelable unit of work   |
//! | [`BoxTaskFuture`] | Return type of [`Task::spawn`]              |
//!
//! ## Flow
//!
//! There are two ways to create a task:
//!
//! **[`TaskFn`]** - pass a name and an async closure.
//!
//! The closure is called on every start and restart, producing a fresh future each time.
//! No struct, no trait impl - just a function. Suitable for the majority of use cases.
//!
//! See examples on [`TaskFn`].
//!
//! **`impl Task`** - define your own struct and implement the [`Task`] trait manually.
//! Gives full control over [`name()`](Task::name) and [`spawn()`](Task::spawn).
//! Use when you need custom initialization, dependency injection, or behavior that a closure cannot express.
//!
//! See example on [`Task`].
//!
//! ## Schema
//!
//! Both paths produce a [`TaskRef`] (`Arc<dyn Task>`), which is then wrapped
//! in a [`TaskSpec`] to configure restart policy, backoff, and timeout.
//!
//! ```text
//! TaskFn::arc(name, closure) ─┐
//!                             ├──► TaskRef ──► TaskSpec ──► Supervisor
//! struct MyTask impl Task    ─┘
//! ```
//!
//! ## Quick start
//!
//! ```rust
//! use taskvisor::{TaskFn, TaskRef, TaskSpec, TaskError};
//! use tokio_util::sync::CancellationToken;
//!
//! let task: TaskRef = TaskFn::arc("worker", |ctx: CancellationToken| async move {
//!     // ... do work, observe ctx.cancelled() ...
//!     Ok::<(), TaskError>(())
//! });
//!
//! let spec = TaskSpec::once(task);         // run once
//! // TaskSpec::restartable(task);          // restart on failure
//! // TaskSpec::with_defaults(task, &cfg);  // inherit from config
//! ```

mod task;
pub use task::{BoxTaskFuture, Task, TaskRef};

mod r#impl;
pub use r#impl::func::TaskFn;

mod spec;
pub use spec::TaskSpec;
