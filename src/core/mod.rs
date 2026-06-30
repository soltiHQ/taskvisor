//! # Runtime core.
//!
//! This module contains the taskvisor runtime implementation.
//!
//! Public API:
//!
//! | File            | Role                                      |
//! |-----------------|-------------------------------------------|
//! | `supervisor.rs` | Public facade and composition root        |
//! | `handle.rs`     | Dynamic task management API               |
//! | `config.rs`     | Runtime defaults and limits               |
//! | `outcome.rs`    | Guaranteed task completion results        |
//! | `builder.rs`    | Runtime construction                      |
//!
//! Internal runtime:
//!
//! | File             | Role                                      |
//! |------------------|-------------------------------------------|
//! | `runtime.rs`     | Owns Bus, Registry, subscribers, shutdown |
//! | `registry.rs`    | Owns active task actors and add/remove    |
//! | `actor.rs`       | Runs one task with restart/backoff policy |
//! | `runner.rs`      | Executes one task attempt                 |
//! | `alive.rs`       | Best-effort live-task snapshot            |
//! | `shutdown.rs`    | OS signal handling                        |
//! | `panic_guard.rs` | Panic boundary for long-lived listeners   |
//!
//! ## Planes
//!
//! taskvisor has three important runtime planes:
//!
//! ```text
//! Management plane:
//!   SupervisorHandle ──► SupervisorCore ──mpsc──► Registry
//!
//! Event plane:
//!   runtime components ──broadcast──► subscriber_listener ──► Subscribe impls
//!
//! Completion plane:
//!   Registry ──oneshot──► TaskWaiter
//! ```
//!
//! The management plane uses an mpsc command channel. Add/remove commands are not delivered through the lossy event bus.
//! The event plane is best-effort and used for logs, metrics, snapshots, and subscriber integrations. Slow consumers can lag and miss events.
//! The completion plane is used by `add_and_watch` and `submit_and_watch`. It is the authoritative final result for one task run.
//!
//! ## Main Flow
//!
//! ```text
//! Supervisor::run(tasks)
//!   ├─ start subscriber listener
//!   ├─ start registry listener
//!   ├─ send initial Add commands
//!   ├─ wait until initial tasks are accepted/rejected
//!   └─ wait for OS shutdown signal or natural completion
//!
//! Supervisor::serve()
//!   ├─ start listeners
//!   └─ return SupervisorHandle
//! ```
//!
//! ## Events
//!
//! Events are published by:
//!
//! - `SupervisorCore`: add/remove/shutdown requests and final runtime verdicts.
//! - `TaskActor`: attempt starts, retry scheduling, actor terminal state.
//! - `SubscriberSet`: subscriber overflow and panic diagnostics.
//! - `Registry`: task added/removed confirmations.
//! - `runner`: per-attempt result events.
//!
//! `AllStoppedWithinGrace` can be emitted after explicit shutdown or after natural completion when all cleanup joins finish within the grace period.
//!
//! ## Notes
//!
//! - `Supervisor::run` is single-shot for one supervisor instance.
//! - Attempts within one `TaskActor` are sequential; the same actor never runs two attempts in parallel.
//! - Event sequence numbers are useful for stale-event filtering and sorting, but they are not a causal ordering guarantee across concurrent producers.
//! - A panic in a long-lived listener is caught and reported as a diagnostic event instead of silently killing the control loop.

mod outcome;
pub use outcome::{TaskOutcome, TaskWaiter};

mod runtime;
pub(crate) use runtime::SupervisorCore;

mod builder;
pub use builder::SupervisorBuilder;

mod config;
pub use config::SupervisorConfig;

mod handle;
pub use handle::SupervisorHandle;

mod supervisor;
pub use supervisor::Supervisor;

mod actor;
mod alive;
mod panic_guard;
mod runner;
mod shutdown;

mod registry;
#[cfg(feature = "controller")]
pub(crate) use registry::OutcomeTx;
