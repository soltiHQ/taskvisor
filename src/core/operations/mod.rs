//! Typed management operations created by [`SupervisorHandle`](super::SupervisorHandle).
//!
//! Builders make admission and completion policy explicit before terminal execution.
//! Default waiting, unwatched add and controller submission operations can be awaited directly;
//! configured operations use their explicit terminal method.
//! Typestate prevents combining mutually exclusive add admission policies while keeping cancellation
//! queue policy independent of its termination deadline.

mod add;
mod cancel;
mod remove;
mod state;
mod target;

pub use add::AddOperation;
pub use cancel::CancelOperation;
pub use remove::RemoveOperation;
pub use state::{
    FailFast, OwnershipTimed, TerminationTimed, TerminationUnbounded, Unwatched, Waiting, Watched,
};
pub use target::TaskTarget;
