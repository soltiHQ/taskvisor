//! Defines the work that Taskvisor runs and supervises.
//!
//! Most applications start with [`TaskFn::arc`]: provide an async closure that creates one attempt,
//! then place the returned [`TaskRef`] in a [`TaskSpec`]. Use the spec constructor that matches the intended lifecycle:
//!
//! - [`TaskSpec::once`] runs at most one attempt;
//! - [`TaskSpec::restartable`] retries retryable failures;
//! - [`TaskSpec::periodic`] repeats after success and may retry retryable failures;
//! - [`TaskSpec::from_defaults`] leaves every execution choice to [`TaskDefaults`](crate::TaskDefaults).
//!
//! Implement [`Task`] directly when a named type is clearer than a closure. Pass the finished spec to a supervisor
//! run method or a management handle. Admission resolves inherited settings before Taskvisor starts the first attempt.
//!
//! ```text
//! application
//!      ├── closure ──► TaskFn ──► TaskRef
//!      └── named type ──► impl Task ──► TaskRef
//!                                      │ TaskSpec
//!                                      ▼
//!                         supervisor or controller admission
//!                                      ▼
//!                                  registry
//!                                      │ TaskDefaults
//!                                      ▼
//!                           TaskActor ──► one attempt at a time
//! ```
//!
//! | Type              | Purpose                                                 |
//! |-------------------|---------------------------------------------------------|
//! | [`Task`]          | Contract for creating one attempt future                |
//! | [`TaskFn`]        | Closure adapter for [`Task`]                            |
//! | [`TaskRef`]       | Shared `Arc<dyn Task>`                                  |
//! | [`TaskSpec`]      | Registration name and execution settings                |
//! | [`TaskSetting`]   | Explicit or inherited setting                           |
//! | [`TaskContext`]   | Cooperative cancellation for one attempt                |
//! | [`BoxTaskFuture`] | Erased future returned by [`Task::spawn`]               |
//!
//! A task object may be used for several attempts or registrations. Every call to [`Task::spawn`]
//! must return a new future. Long-running attempts must observe [`TaskContext`] cancellation.
//! A timeout drops the attempt future. Force-abort asks Tokio to drop it after the current poll returns.
//! Neither action undoes external side effects or interrupts synchronous code.

mod task;
pub use task::{BoxTaskFuture, Task, TaskRef};

mod spec;
pub use spec::{TaskSetting, TaskSpec};

mod context;
pub use context::TaskContext;

mod r#impl;
pub use r#impl::func::TaskFn;
