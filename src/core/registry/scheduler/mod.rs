//! Starts admitted actors and retains force-aborted actors until physical exit.
//!
//! Admission creates a [`ScheduledActor`] and its registry-owned [`ActorHandle`]
//! before it writes the indexes. [`ActorRuntime`] spawns the actor only after that commit.
//! The wrapper sends a reliable result before its completion identity.
//! Removal can claim a ready result without using lifecycle events.
//!
//! ```text
//! admission commit ──► scheduled actor ──► actor wrapper
//!                                              ├── completed ──► completion identity
//!                                              └── force-aborted ──► attempt reaper
//! terminal commit ──► attempt reaper ──► deferred cleanup
//! ```
//!
//! Force-abort transfers physical ownership to [`AttemptReaper`] before requesting abort.
//! The reaper keeps the label and activity state until the actor output and terminal cleanup
//! bundle meet. This prevents a replacement from overlapping a physically active attempt.
//! Registry shutdown does not wait for a blocked reaper attempt.
//! The host Tokio runtime is the outer lifetime boundary.

mod actor;
mod reaper;
mod runtime;

pub(in crate::core::registry) use actor::{
    ActorHandle, ActorJoinError, ActorRegistration, ScheduledActor,
};
pub(in crate::core::registry) use reaper::AttemptReaper;
#[cfg(test)]
pub(in crate::core::registry) use reaper::AttemptReservation;
pub(in crate::core::registry) use runtime::ActorRuntime;
