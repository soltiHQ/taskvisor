//! Routes task management through the bounded registry command queue.
//!
//! [`SupervisorHandle`](crate::SupervisorHandle), static run workflows, and the optional controller enter here.
//! State-changing operations pass through the command admission gate before they commit to the registry queue.
//! Queries read the registry directly.
//!
//! ```text
//! SupervisorHandle / static run / controller
//!                     ▼
//!          SupervisorCore management
//!             ├── add ───────────────► cleanup slot ──► command gate ──► registry queue
//!             ├── remove or cancel ──► command gate ──► registry queue
//!             └── query ─────────────► Registry
//! ```
//!
//! A direct add reserves cleanup ownership before the runtime accepts its task value.
//! Controller admission can reserve registry queue capacity while the controller still owns that value.
//! Every mutating path performs its final shutdown check at commit. Registry reply channels carry authoritative decisions;
//! best-effort events do not replace them.

mod add;
mod cancel;
mod command_gate;
mod ownership;
mod query;
mod remove;

#[cfg(feature = "controller")]
pub(crate) use add::ControllerAddPermit;
