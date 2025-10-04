//! # Task abstractions and specifications.
//!
//! This module provides the core task-related types:
//! - [`Task`] - trait for implementing async cancelable tasks
//! - [`TaskFn`] - function-based task implementation
//! - [`TaskRef`] - shared reference to a task (`Arc<dyn Task>`)
//! - [`TaskSpec`] - specification bundling task with policies

mod r#impl;
mod spec;
mod task;

pub use r#impl::func::TaskFn;
pub use spec::TaskSpec;
pub use task::{Task, TaskRef};
