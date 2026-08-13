//! Implements the runtime behind Taskvisor's public supervision API.
//!
//! Applications build a [`Supervisor`], choose a static or dynamic entry path,
//! and describe work with [`TaskSpec`](crate::TaskSpec). Static `run*` methods
//! add an initial batch and start shutdown when registry membership becomes
//! empty. [`Supervisor::serve`] returns a [`SupervisorHandle`] for work added and
//! managed while the application is running. The two paths can share one
//! runtime when `serve` is called before the single static run.
//!
//! ```text
//! application ──► SupervisorBuilder ──► Supervisor
//!                                             ├── run* ──► initial task batch
//!                                             └── serve ──► SupervisorHandle
//!                                                                  │ commands
//!                                                                  ▼
//!                                                               registry
//!                                                                  │ task
//!                                                                  ▼
//!                                                              TaskActor
//!                                                                  │
//!                                                                  ▼
//!                                                        sequential attempts
//! ```
//!
//! The registry is the source of truth for registered membership and task
//! activity. Direct replies confirm management decisions. A [`TaskWaiter`]
//! receives one final [`TaskOutcome`] through a reliable in-process channel.
//! Events use a separate best-effort observability path and never confirm state.
//!
//! Shutdown closes admission before it drains tasks and runtime workers. The
//! last public owner can only start non-blocking cancellation from `Drop`; use
//! [`SupervisorHandle::shutdown`] when the final shutdown result is required.

mod outcome;
pub use outcome::{TaskOutcome, TaskOutcomeKind, TaskWaiter};

mod runtime;
pub(crate) use runtime::SupervisorCore;

mod builder;
pub use builder::SupervisorBuilder;

mod config;
pub(crate) use config::MAX_ASYNC_CAPACITY;
#[cfg(feature = "controller")]
pub(crate) use config::validate_async_capacity;
pub use config::{ConfigError, SupervisorConfig};

mod task_defaults;
pub use task_defaults::TaskDefaults;

mod handle;
pub use handle::SupervisorHandle;

mod supervisor;
pub use supervisor::Supervisor;

mod owner;
pub(crate) use owner::RuntimeOwner;

pub(crate) mod deferred_drop;
pub(crate) mod panic_guard;

mod actor;
mod runner;
mod shutdown;

mod registry;
#[cfg(feature = "controller")]
pub(crate) use registry::{AddReplyRx, OutcomeTx, RemovalCompletion};
#[cfg(feature = "controller")]
pub(crate) use runtime::ControllerAddPermit;

/// Controller add payload returned intact when registry command admission does not commit.
///
/// Keeping the user task and outcome sender together lets the controller restore
/// ownership before any user-provided destructor can run.
#[cfg(feature = "controller")]
pub(crate) struct UncommittedWatchedAdd {
    /// Registry error that prevented command commit.
    pub(crate) error: crate::RuntimeError,
    /// Stable task name prepared for registry admission.
    pub(crate) label: std::sync::Arc<str>,
    /// Task specification and its reserved cleanup ownership.
    pub(crate) owned: deferred_drop::OwnedTask<crate::TaskSpec>,
    /// Watched-outcome sender returned to controller ownership.
    pub(crate) done: Option<registry::OutcomeTx>,
}

#[cfg(feature = "controller")]
impl std::fmt::Debug for UncommittedWatchedAdd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UncommittedWatchedAdd")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
