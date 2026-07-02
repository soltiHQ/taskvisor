#[cfg(feature = "logging")]
mod log;
#[cfg(feature = "logging")]
pub use log::LogWriter;

#[cfg(feature = "tracing")]
mod tracing;
#[cfg(feature = "tracing")]
pub use self::tracing::TracingBridge;
