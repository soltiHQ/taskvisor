//! Turns add commands into committed registry entries.
//!
//! The registry listener calls this package for single adds and static batches. Admission checks name
//! ownership and the registration limit. It builds actors outside the state write lock.
//! It repeats the checks after acquiring the write lock because preparation did not hold registry state.
//!
//! ```text
//! add command ──► initial check ──► prepare actor
//!                                      ▼
//!                                 locked check
//!                                      ├── reject ──► direct error or outcome
//!                                      └── accept ──► index ──► scheduler
//!                                                               ▼
//!                                                        open start gate
//! ```
//!
//! The start gate keeps task bodies behind the registry decision.
//! A rejected add enters no index and starts no task body.
//! A batch commits every item or none.
//! Rejected watched tasks receive their outcome through the direct outcome path.

use std::sync::{Arc, atomic::AtomicBool};

use tokio::sync::watch;

use super::{
    Registry,
    completion::{OutcomeTx, RemovalCompletion},
    scheduler::{ActorHandle, ActorRegistration, ScheduledActor},
};
use crate::{
    core::{
        actor::{ActorExitReason, TaskActor, TaskActorParams, TaskActorResources},
        deferred_drop::{DropBundle, OwnedTask},
        outcome::TaskOutcome,
    },
    identity::TaskId,
    tasks::TaskSpec,
};

mod batch;
mod single;

/// Owns one prepared actor until admission commits or rejects it.
struct PreparedRegistration {
    /// Stable identity assigned to the task.
    id: TaskId,
    /// Name reserved by the registry entry.
    name: Arc<str>,
    /// Actor handle retained by the registry entry.
    join: ActorHandle,
    /// Token used to stop the actor.
    cancel: tokio_util::sync::CancellationToken,
    /// Optional sender for the watched task outcome.
    done: Option<OutcomeTx>,
    /// Signal completed by terminal registry cleanup.
    completion: RemovalCompletion,
    /// Actor waiting to be submitted to the scheduler.
    scheduled: ScheduledActor,
    /// Ownership bundle for deferred user-value destruction.
    cleanup: DropBundle,
    /// Shared running-attempt state.
    activity: Arc<AtomicBool>,
}

/// Delivers a rejection or attaches it to the existing cleanup bundle.
///
/// The cleanup bundle owns the outcome when no receiver accepts it.
fn deliver_or_attach_rejection(
    done: Option<OutcomeTx>,
    outcome: TaskOutcome,
    cleanup: &mut DropBundle,
) {
    match done {
        Some(done) => {
            if let Err(undelivered) = done.send(outcome) {
                cleanup.attach_outcome(undelivered);
            }
        }
        None => cleanup.attach_outcome(outcome),
    }
}

impl Registry {
    /// Resolves task defaults and builds an actor outside the state lock.
    ///
    /// The actor is not spawned and remains behind `start`.
    fn prepare_registration(
        &self,
        id: TaskId,
        name: Arc<str>,
        owned: OwnedTask<TaskSpec>,
        done: Option<OutcomeTx>,
        completion: Option<RemovalCompletion>,
        mut start: watch::Receiver<bool>,
    ) -> PreparedRegistration {
        let task_token = self.runtime_token.child_token();
        let (spec, cleanup) = owned.into_parts();
        let spec = spec.resolve(&self.task_defaults);
        let task = spec.task().clone();
        let activity = Arc::new(AtomicBool::new(false));
        let cleanup_poisoned = Arc::new(AtomicBool::new(false));
        let completion = completion.unwrap_or_else(RemovalCompletion::new);

        let actor = TaskActor::new(
            self.bus.clone(),
            Arc::clone(&name),
            task,
            TaskActorParams {
                restart: spec.restart(),
                backoff: spec.backoff(),
                timeout: spec.timeout(),
                max_retries: spec.max_retries(),
            },
            TaskActorResources {
                semaphore: self.semaphore.clone(),
                activity: Arc::clone(&activity),
                cleanup_poisoned: Arc::clone(&cleanup_poisoned),
            },
            id,
        );

        let task_token_clone = task_token.clone();
        let actor_future = async move {
            loop {
                if *start.borrow_and_update() {
                    break;
                }
                if start.changed().await.is_err() {
                    return ActorExitReason::Canceled;
                }
            }
            actor.run(task_token_clone).await
        };
        let (scheduled, join) = ScheduledActor::new(
            ActorRegistration {
                id,
                name: Arc::clone(&name),
                activity: Arc::clone(&activity),
                cleanup_poisoned,
                physical_release: completion.clone(),
                reaper: self.actors.attempt_reaper(),
                completion_tx: self.listener.completion_tx.clone(),
            },
            actor_future,
        );

        PreparedRegistration {
            id,
            name,
            join,
            cancel: task_token,
            done,
            completion,
            scheduled,
            cleanup,
            activity,
        }
    }

    /// Returns the registration limit when an incoming set would exceed it.
    ///
    /// Force-aborted attempts remain counted while registry ownership is being removed.
    /// This keeps the bound strict without sharing registry and cleanup locks.
    fn registered_limit_exceeded(&self, current: usize, incoming: usize) -> Option<usize> {
        let limit = self.max_registered_tasks?.get();
        let exceeds = current
            .checked_add(self.actors.reaping_attempts())
            .and_then(|used| used.checked_add(incoming))
            .is_none_or(|total| total > limit);
        exceeds.then_some(limit)
    }
}
