//! Convenience re-exports for common use.
//!
//! Importing the prelude brings all commonly needed types into scope:
//!
//! ```rust
//! use taskvisor::prelude::*;
//! ```
//!
//! This includes the core types for defining tasks, configuring the supervisor,
//! subscribing to events, and the `CancellationToken` from `tokio-util` that
//! every task implementation needs.

// Core
pub use crate::core::{Supervisor, SupervisorConfig};

// Tasks
pub use crate::tasks::{Task, TaskFn, TaskRef, TaskSpec};

// Policies
pub use crate::policies::{BackoffPolicy, RestartPolicy};

// Events
pub use crate::events::{Event, EventKind};

// Subscribers
pub use crate::subscribers::Subscribe;

// Errors
pub use crate::error::{RuntimeError, TaskError};

// Re-export CancellationToken — every task needs it.
pub use tokio_util::sync::CancellationToken;
