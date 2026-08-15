//! Stores the registry's authoritative membership and removal phase.
//!
//! Admission inserts one [`Entry`] into the identity map and label index while holding a single write lock.
//! Remove, cancel, natural completion, and shutdown compete for the same transition:
//!
//! ```text
//! registered ──► removing ──► absent
//! ```
//!
//! The winning claim moves the only [`ActorHandle`] out of the entry. Both indexes remain until terminal
//! commit finishes the removing phase. Activity bits serve physical-attempt queries. [`HandleCleanup`] is
//! a fallback that keeps user values on the reserved cleanup path if normal removal does not extract them.

use std::{
    collections::HashMap,
    sync::{Arc, atomic::AtomicBool},
};

use tokio_util::sync::CancellationToken;

use super::completion::{OutcomeTx, RemovalCompletion};
use super::scheduler::{ActorHandle, AttemptReaper};
use crate::{core::deferred_drop::DropBundle, identity::TaskId};

/// Owns the actor and terminal data while an entry is registered.
pub(super) struct Handle {
    /// Physical actor handle claimed by exactly one removal owner.
    join: ActorHandle,
    /// Cooperative cancellation token for the registered actor.
    pub(super) cancel: CancellationToken,
    /// Optional sender for the watched terminal outcome.
    pub(super) done: Option<OutcomeTx>,
    /// Shared logical and physical release latches.
    pub(super) completion: RemovalCompletion,
    /// Keeps the user task and its cleanup capacity until terminal cleanup.
    ///
    /// The wrapper prevents the final library-owned `Arc` from running a user destructor on the actor task or registry listener.
    cleanup: HandleCleanup,
}

impl Handle {
    pub(super) fn new(
        join: ActorHandle,
        cancel: CancellationToken,
        done: Option<OutcomeTx>,
        completion: RemovalCompletion,
        cleanup: HandleCleanup,
    ) -> Self {
        Self {
            join,
            cancel,
            done,
            completion,
            cleanup,
        }
    }

    /// Returns whether the actor result is ready without waiting.
    pub(super) fn result_ready(&mut self) -> bool {
        self.join.result_ready()
    }

    pub(super) fn join_mut(&mut self) -> &mut ActorHandle {
        &mut self.join
    }

    /// Transfers physical ownership to the reaper and requests abort.
    pub(super) fn abort(&mut self) {
        self.join.abort();
    }

    /// Separates reporting data only after the actor is physically joined or has already transferred itself to the reaper.
    ///
    /// Dropping `join` first preserves the same ownership ordering as ordinary `Handle` teardown before the reserved cleanup bundle is extracted.
    pub(super) fn into_report_parts(self) -> (Option<OutcomeTx>, DropBundle) {
        let Self {
            join,
            done,
            cleanup,
            ..
        } = self;
        drop(join);
        (done, cleanup.into_bundle())
    }
}

/// Keeps raw registry teardown connected to force-abort tracking.
///
/// `Handle::join` is declared before this field.
/// Ordinary field teardown first lets `ActorHandle::drop` register physical ownership.
pub(super) struct HandleCleanup {
    /// Runtime identity used to find the matching reaper record.
    id: TaskId,
    /// Physical owner that pairs this bundle with actor release.
    reaper: AttemptReaper,
    /// Release latch shared with the authoritative entry.
    completion: RemovalCompletion,
    /// Reserved cleanup bundle extracted by the winning removal claim.
    bundle: Option<DropBundle>,
}

impl HandleCleanup {
    pub(super) fn new(
        id: TaskId,
        reaper: AttemptReaper,
        completion: RemovalCompletion,
        bundle: DropBundle,
    ) -> Self {
        Self {
            id,
            reaper,
            completion,
            bundle: Some(bundle),
        }
    }

    /// Extracts the reserved cleanup bundle for normal removal.
    pub(super) fn into_bundle(mut self) -> DropBundle {
        self.bundle
            .take()
            .expect("registry handle owns one terminal cleanup bundle")
    }
}

impl Drop for HandleCleanup {
    fn drop(&mut self) {
        let Some(bundle) = self.bundle.take() else {
            return;
        };
        self.reaper
            .attach_terminal(self.id, bundle, None, self.completion.clone());
        self.completion.complete_logical();
    }
}

/// Lifecycle phase of one authoritative registry entry.
pub(super) enum EntryState {
    /// The actor can still be claimed by remove, completion, or shutdown.
    Registered(Box<Handle>),
    /// One owner has the actor handle and must commit terminal removal.
    Removing {
        /// Shared release latches retained after the actor handle moves out.
        completion: RemovalCompletion,
    },
}

/// Authoritative membership record kept until terminal join cleanup finishes.
pub(super) struct Entry {
    /// Canonical task label reserved by this entry.
    pub(super) label: Arc<str>,
    /// Authoritative indication that this task is currently inside an attempt.
    pub(super) activity: Arc<AtomicBool>,
    /// Current membership and actor-ownership phase.
    pub(super) state: EntryState,
}

/// Registry indexes guarded by one lock.
///
/// Keeping both maps under the same lock keeps identity and label lookup in sync.
#[derive(Default)]
pub(super) struct Inner {
    /// Canonical task map keyed by runtime identity.
    ///
    /// Entries stay here in both `Registered` and `Removing` phases.
    pub(super) tasks: HashMap<TaskId, Entry>,

    /// Label lookup used for conflict checks and label-based operations.
    pub(super) by_label: HashMap<Arc<str>, TaskId>,
}
