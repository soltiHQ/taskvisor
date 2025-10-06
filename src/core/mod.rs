//! Runtime core: orchestration and lifecycle.

mod actor;
mod alive;
mod registry;
mod runner;
mod shutdown;
mod supervisor;

pub use supervisor::Supervisor;