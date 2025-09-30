//! # Task abstractions and specifications.
//!
//! This module provides the core task-related types:
//! - [`Task`] - trait for implementing async cancelable tasks
//! - [`TaskFn`] - function-based task implementation
//! - [`TaskRef`] - shared reference to a task (`Arc<dyn Task>`)
//! - [`TaskSpec`] - specification bundling task with policies

mod spec;
mod task;
mod task_fn;

pub use spec::TaskSpec;
pub use task::Task;
pub use task_fn::{TaskFn, TaskRef};
