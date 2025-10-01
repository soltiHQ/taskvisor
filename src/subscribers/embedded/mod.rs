//! # Built-in subscribers
//!
//! These are small, self-contained implementations useful for demos and runtime
//! internals.
//!
//! - [`LogWriter`]: prints events in a human-readable form (demo/debug).
//! - [`AliveTracker`]: tracks currently running tasks by logical name.

mod alive;
mod log;

pub use alive::AliveTracker;
pub use log::LogWriter;
