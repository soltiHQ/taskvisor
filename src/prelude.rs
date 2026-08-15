//! Re-exports the types most Taskvisor applications use together.
//!
//! Start here in application modules that use several Taskvisor APIs:
//!
//! ```rust
//! use taskvisor::prelude::*;
//! ```
//!
//! ```text
//! application module ──► taskvisor::prelude
//!                              ├── core API ──────────────► always available
//!                              ├── controller API ────────► controller feature
//!                              ├── Subscribe ─────────────► always available
//!                              ├── built-in subscribers ──► logging or tracing feature
//!                              └── cancellation token ────► tokio-util-interop feature
//! ```
//!
//! This module only shortens imports. It creates no runtime state and changes no behavior.
//! Prefer explicit crate-root imports when a smaller local namespace or clearer dependencies matter.

/// Supervisor construction, control, configuration, and final outcomes.
pub use crate::core::{
    ConfigError, Supervisor, SupervisorBuilder, SupervisorConfig, SupervisorHandle, TaskDefaults,
    TaskOutcome, TaskOutcomeKind, TaskWaiter,
};

/// Task implementation, context, shared references, and specifications.
pub use crate::tasks::{BoxTaskFuture, Task, TaskContext, TaskFn, TaskRef, TaskSetting, TaskSpec};

/// Restart decisions and retry-delay policies.
pub use crate::policies::{BackoffError, BackoffPolicy, JitterPolicy, RestartPolicy};

/// Best-effort event values and machine-readable categories.
pub use crate::events::{BackoffSource, Event, EventKind, RejectionKind};

/// Build, runtime, task, and combined error types.
pub use crate::error::{BuildError, Error, RuntimeError, TaskError};

/// Application callback interface for best-effort events.
pub use crate::subscribers::Subscribe;

/// Process-local task submission identity.
pub use crate::identity::TaskId;

/// Keyed queue, replace, and reject admission types.
///
/// Requires the `controller` feature.
#[cfg(feature = "controller")]
#[cfg_attr(docsrs, doc(cfg(feature = "controller")))]
pub use crate::controller::{
    AdmissionPolicy, ControllerConfig, ControllerError, ControllerSnapshot, ControllerSpec,
    PreparedSubmission, SlotStatusKind, SlotView,
};

/// Built-in standard logging subscriber.
///
/// Requires the `logging` feature.
#[cfg(feature = "logging")]
#[cfg_attr(docsrs, doc(cfg(feature = "logging")))]
pub use crate::subscribers::LogWriter;

/// Built-in bridge from Taskvisor events to `tracing`.
///
/// Requires the `tracing` feature.
#[cfg(feature = "tracing")]
#[cfg_attr(docsrs, doc(cfg(feature = "tracing")))]
pub use crate::subscribers::{TracingBridge, TracingBridgeWithReasons};

/// Tokio's raw cancellation token for explicit interoperability.
///
/// Requires the `tokio-util-interop` feature.
/// By default, public task code should use [`TaskContext`] instead.
#[cfg(feature = "tokio-util-interop")]
#[cfg_attr(docsrs, doc(cfg(feature = "tokio-util-interop")))]
pub use tokio_util::sync::CancellationToken;
