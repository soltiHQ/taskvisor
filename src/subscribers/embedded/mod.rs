//! # Built-in subscribers
//!
//! These are small, self-contained implementations useful for demos and runtime
//! internals.
//!
//! - [`LogWriter`]: prints events in a human-readable form (demo/debug).

mod log;

pub use log::LogWriter;
