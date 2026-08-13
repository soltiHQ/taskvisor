//! Verifies the controller engine across intake, slot admission, runtime
//! results, identity operations, snapshots, and shutdown.
//!
//! ```text
//! test fixtures
//!      ├── commands ──► controller engine
//!      ├── registry results ──► controller engine
//!      └── assertions ──► state, outcomes, events, and cleanup
//! ```
//!
//! Each sibling module focuses on one boundary or lifecycle transition.

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
