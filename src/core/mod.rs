//! # Runtime API
//!
//! This module contains the public runtime types:
//!
//! - [`Supervisor`] and [`SupervisorBuilder`] create and start a runtime.
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
//! | Observability                                                            | runtime components -> event bus -> event relay -> alive tracker and subscribers |
//!
//! The registry is the source of truth for registered identities and names.
//! Events are for observability and may be lost when consumers are slow.
//! Alive snapshots are based on those events. They can lag or miss state.
//!
//! ## When a Command Returns
//!
//! | Operation  | What the return value confirms                                                              |
//! |------------|---------------------------------------------------------------------------------------------|
//! | `add*`     | The registry accepted or rejected the task. The first attempt may not have started yet.     |
//! | `remove*`  | Whether this caller claimed the stop request. Registered task cleanup may still be running. |
//! | `cancel*`  | Known work reached terminal cleanup, unless the caller's explicit wait timeout expired.     |
//! | `shutdown` | Shared runtime cleanup finished. The result reports its final status.                       |
//!
//! Regular management methods wait for capacity in every bounded queue they use.
//! Their `try_*` versions fail fast at those queue boundaries.
//!
//! After a command is accepted, both forms may still wait for a direct decision or terminal cleanup.
//! [`SupervisorHandle::list`] reads authoritative registry membership.
//! [`SupervisorHandle::alive_snapshot`] and [`SupervisorHandle::is_alive`] are best-effort views built from events.
//!
//! ## Important Rules
//!
//! - [`Supervisor::run`] is single-shot and registers its initial tasks as one batch.
//! - Attempts for one registered task are sequential.
//! - New task admission closes when shutdown starts.
//! - Explicit shutdown returns after task, listener, and subscriber cleanup.
//! - Dropping the last public owner only starts best-effort cancellation.
//! - Event sequence numbers help sort observations, but do not prove causal order between concurrent tasks.

mod outcome;
pub use outcome::{TaskOutcome, TaskWaiter};

mod runtime;
pub(crate) use runtime::SupervisorCore;

mod builder;
pub use builder::SupervisorBuilder;

mod config;
pub use config::{ConfigError, SupervisorConfig};

mod task_defaults;
pub use task_defaults::TaskDefaults;

mod handle;
pub use handle::SupervisorHandle;

mod supervisor;
pub use supervisor::Supervisor;

mod owner;
pub(crate) use owner::RuntimeOwner;

pub(crate) mod panic_guard;

mod actor;
mod alive;
mod runner;
mod shutdown;

mod registry;
#[cfg(feature = "controller")]
pub(crate) use registry::{AddReplyRx, OutcomeTx, RemovalCompletion};
