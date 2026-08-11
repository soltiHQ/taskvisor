//! # Runtime API
//!
//! This module contains the public runtime types:
//!
//! - [`Supervisor`] and [`SupervisorBuilder`] create and start a runtime.
//! - [`BuildError`](crate::BuildError) reports process-wide ownership exhaustion while building.
//! - [`SupervisorHandle`] manages tasks in dynamic mode.
//! - [`SupervisorConfig`] sets runtime limits.
//! - [`TaskDefaults`] sets inherited task behavior.
//! - [`TaskWaiter`] returns a final [`TaskOutcome`].
//!
//! ## Runtime Paths
//!
//! State changes, final results, and observations use different paths:
//!
//! | Path                                                                     | Route                                                                           |
//! |--------------------------------------------------------------------------|---------------------------------------------------------------------------------|
//! | Direct `add*`, label operations, and identity stops without a controller | `SupervisorHandle` -> registry -> direct reply                                  |
//! | `submit*` and identity stops with a controller                           | `SupervisorHandle` -> controller -> registry when needed                        |
//! | Watched final result                                                     | registry or controller -> direct one-shot -> `TaskWaiter`                       |
//! | Observability                                                            | runtime components -> bounded event ingress -> event relay -> subscribers       |
//! | Attempt activity                                                         | attempt guard -> registry activity bit -> handle query                          |
//!
//! The registry is the source of truth for registered identities and names.
//! Events are for observability and may be lost when consumers are slow.
//! Attempt-activity queries are backed by registry state and are independent of events.
//!
//! ## When a Command Returns
//!
//! | Operation  | What the return value confirms                                                              |
//! |------------|---------------------------------------------------------------------------------------------|
//! | `add*`     | The registry accepted or rejected the task. The first attempt may not have started yet.     |
//! | `remove*`  | Whether this caller claimed the stop request. Registered task cleanup may still be running. |
//! | `cancel*`  | Known work reached logical terminal cleanup, unless the explicit wait timeout expired.     |
//! | `shutdown` | The bounded shared cleanup workflow finished. The result reports its final status.          |
//!
//! Regular management methods wait for capacity in every bounded queue they use.
//! Their `try_*` versions fail fast at those queue boundaries.
//!
//! After a command is accepted, both forms may still wait for a direct decision or terminal cleanup.
//! [`SupervisorHandle::list`] reads authoritative registry membership.
//! [`SupervisorHandle::alive_snapshot`] and [`SupervisorHandle::is_alive`] read authoritative attempt activity from registry entries.
//!
//! ## Important Rules
//!
//! - Static run methods are single-shot and register their initial tasks as one batch.
//! - [`Supervisor::run`] and [`Supervisor::run_until`] do not install process signal handlers.
//! - Attempts for one registered task are sequential.
//! - New task admission closes when shutdown starts.
//! - Explicit shutdown completes the bounded task, listener, and subscriber cleanup workflow.
//! - A force-reaped synchronous task, detached subscriber callback, or isolated user destructor can remain physically active afterward.
//! - Dropping the last public owner only starts best-effort cancellation.
//! - Event sequence numbers help sort observations, but do not prove causal order between concurrent tasks.

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
pub(crate) mod task_metadata;

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
    pub(crate) error: crate::RuntimeError,
    pub(crate) label: std::sync::Arc<str>,
    pub(crate) owned: deferred_drop::OwnedTask<crate::TaskSpec>,
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
