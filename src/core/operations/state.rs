//! Public marker types used by management-operation typestate.

use std::time::Duration;

/// The operation waits for ownership and command capacity.
#[derive(Clone, Copy, Debug, Default)]
pub struct Waiting;

/// The operation fails immediately when bounded admission capacity is unavailable.
#[derive(Clone, Copy, Debug, Default)]
pub struct FailFast;

/// Ownership admission waits at most the contained duration.
#[derive(Clone, Copy, Debug)]
pub struct OwnershipTimed(pub(crate) Duration);

/// The operation does not return a final-outcome waiter.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unwatched;

/// The operation returns a final-outcome waiter.
#[derive(Clone, Copy, Debug, Default)]
pub struct Watched;

/// Cancellation waits without a caller-provided termination deadline.
#[derive(Clone, Copy, Debug, Default)]
pub struct TerminationUnbounded;

/// Cancellation limits only the caller's wait for terminal cleanup.
#[derive(Clone, Copy, Debug)]
pub struct TerminationTimed(pub(crate) Duration);
