//! Provides ready-made subscribers for common output systems.
//!
//! These implementations sit at the end of the ordinary subscriber path and use the same queue,
//! panic, and shutdown rules as every [`Subscribe`](crate::subscribers::Subscribe) implementation.
//!
//! ```text
//! subscriber callback lane
//!      ▼
//! embedded subscriber
//!      ├── logging ──► LogWriter ──────► standard output
//!      └── tracing ──► TracingBridge ──► tracing event
//! ```
//!
//! Enable `logging` for readable standard output.
//! Enable `tracing` to emit structured fields into the application's active tracing dispatcher.
//! The parent module re-exports each type when its feature is enabled.
#[cfg(feature = "logging")]
mod log;
#[cfg(feature = "logging")]
pub use log::LogWriter;

#[cfg(feature = "tracing")]
mod tracing;
#[cfg(feature = "tracing")]
pub use self::tracing::{TracingBridge, TracingBridgeWithReasons};
