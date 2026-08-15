//! Verifies the controller engine across intake, slot admission, runtime results, identity operations, snapshots, and shutdown.

mod support;

mod admission;
mod capacity;
mod completion;
mod handle;
mod identity;
mod lifecycle;
mod queue;
mod replacement;
mod shutdown;
mod snapshot;
mod state;
