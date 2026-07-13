//! # Common imports
//!
//! Import the prelude when building ordinary Taskvisor applications:
//!
//! ```rust
//! use taskvisor::prelude::*;
//! ```
//!
//! The prelude includes the main runtime, task, policy, event, subscriber, error, and identity types.
//! It does not change behavior or enable features.

/// Core supervisor runtime.
pub use crate::core::{
    ConfigError, Supervisor, SupervisorBuilder, SupervisorConfig, SupervisorHandle, TaskDefaults,
    TaskOutcome, TaskWaiter,
};

/// Task abstractions and task specs.
pub use crate::tasks::{BoxTaskFuture, Task, TaskContext, TaskFn, TaskRef, TaskSpec};

/// Restart, retry, backoff, and jitter policies.
pub use crate::policies::{BackoffError, BackoffPolicy, JitterPolicy, RestartPolicy};

/// Runtime event types.
pub use crate::events::{BackoffSource, Event, EventKind};

/// Runtime and task error types.
pub use crate::error::{Error, RuntimeError, TaskError};

/// Event subscriber trait.
pub use crate::subscribers::Subscribe;

/// Runtime task identity.
pub use crate::identity::TaskId;

/// Slot-based admission controller types.
///
/// Requires the `controller` feature.
#[cfg(feature = "controller")]
pub use crate::controller::{
    AdmissionPolicy, ControllerConfig, ControllerError, ControllerSnapshot, ControllerSpec,
    SlotStatusKind, SlotView,
};

/// Built-in logging subscriber.
///
/// Requires the `logging` feature.
#[cfg(feature = "logging")]
pub use crate::subscribers::LogWriter;

/// Built-in tracing bridge subscriber.
///
/// Requires the `tracing` feature.
#[cfg(feature = "tracing")]
pub use crate::subscribers::TracingBridge;

/// Raw cancellation-token interop.
///
/// Requires the `tokio-util-interop` feature.
/// By default, public task code should use [`TaskContext`] instead.
#[cfg(feature = "tokio-util-interop")]
pub use tokio_util::sync::CancellationToken;
