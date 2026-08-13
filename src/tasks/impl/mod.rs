//! Adapters from application code to the [`Task`](crate::Task) contract.
//!
//! This private implementation layer is re-exported through [`crate::tasks`].
//! It currently provides [`TaskFn`](crate::TaskFn), which turns an async closure
//! into a reusable task object.
//!
//! ```text
//! async closure ──► TaskFn ──► impl Task ──► TaskRef
//! ```
pub mod func;
