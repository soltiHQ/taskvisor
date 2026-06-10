//! Convenience re-exports for common use.
//!
//! Importing the prelude brings all commonly needed types into scope:
//!
//! ```rust
//! use taskvisor::prelude::*;
//! ```

// Core
pub use crate::core::{Supervisor, SupervisorConfig, SupervisorHandle};

// Tasks
pub use crate::tasks::{BoxTaskFuture, Task, TaskFn, TaskRef, TaskSpec};

// Policies
pub use crate::policies::{BackoffPolicy, JitterPolicy, RestartPolicy};

// Events
pub use crate::events::{Event, EventKind};

// Subscribers
pub use crate::subscribers::Subscribe;

// Errors
pub use crate::error::{RuntimeError, TaskError};

// Runtime task identity
pub use crate::identity::TaskId;

// Re-export CancellationToken.
pub use tokio_util::sync::CancellationToken;
