//! Owns the transition from registry membership to terminal cleanup.
//!
//! Remove, cancel, actor completion, and shutdown can all request this transition.
//! They compete under the state lock.
//! One winner changes the entry to removing and takes its actor handle.
//! The name remains reserved during the join.
//!
//! ```text
//! command or completion ──► claim ────────────────────► actor join ───► terminal commit
//! shutdown ───────────────► claim remaining entries ──► actor joins ──► terminal commit
//! ```
//!
//! `commands` defines remove and cancel decisions.
//! `join` waits for actors outside the listener and handles the shared shutdown deadline.
//! `terminal` removes both indexes, reports the outcome, and completes logical waiters.
//! `pending` tracks every claimed removal owner for shutdown.
//! Force-aborted physical ownership continues in `scheduler` until it can enter deferred cleanup.

use crate::{
    core::{
        actor::ActorExitReason,
        deferred_drop::DropBundle,
        registry::{
            completion::{OutcomeTx, RemovalCompletion},
            scheduler::ActorJoinError,
        },
    },
    identity::TaskId,
};

mod commands;
mod join;
mod pending;
mod terminal;

pub(super) use pending::PendingJoins;
#[cfg(test)]
pub(super) use terminal::TerminalFinalizer;

/// Terminal join state passed to registry cleanup.
pub(super) enum JoinCompletion {
    /// Result returned by the actor join handle.
    Joined(Result<ActorExitReason, ActorJoinError>),
    /// Removal transferred an unfinished actor to the force-abort tracker.
    ForceAborted,
}

/// Values required to commit one actor's terminal cleanup.
pub(super) struct RemovalReport {
    /// Identity of the removing registry entry.
    pub(super) id: TaskId,
    /// Optional sender for a watched task outcome.
    pub(super) outcome: Option<OutcomeTx>,
    /// Result produced while removing the actor.
    pub(super) join: JoinCompletion,
    /// Two-phase completion for this removal.
    pub(super) completion: RemovalCompletion,
    /// User-owned values retained for isolated destruction.
    pub(super) cleanup: DropBundle,
}
