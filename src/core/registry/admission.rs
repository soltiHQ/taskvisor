//! Actor preparation, scheduling, and atomic single/batch admission.

use std::{
    collections::HashSet,
    sync::{Arc, atomic::AtomicBool},
};

use tokio::sync::{oneshot, watch};

use super::{
    Registry,
    completion::{OutcomeTx, RemovalCompletion},
    protocol::{AddBatchItem, AddReply},
    scheduler::{ActorHandle, ActorRegistration, ScheduledActor},
    state::{Entry, EntryState, Handle, HandleCleanup},
};
use crate::{
    core::outcome::TaskOutcome,
    core::{
        actor::{ActorExitReason, TaskActor, TaskActorParams, TaskActorResources},
        deferred_drop::{DropBundle, OwnedTask},
    },
    error::RuntimeError,
    events::{Event, EventKind, RejectionKind},
    identity::TaskId,
    reasons,
    tasks::TaskSpec,
};

/// Fully prepared actor registration that has not entered the authoritative indexes yet.
struct PreparedRegistration {
    id: TaskId,
    label: Arc<str>,
    join: ActorHandle,
    cancel: tokio_util::sync::CancellationToken,
    done: Option<OutcomeTx>,
    completion: RemovalCompletion,
    scheduled: ScheduledActor,
    cleanup: DropBundle,
    activity: Arc<AtomicBool>,
}

/// Delivers a pre-registration rejection or charges its destruction to the
/// submission's existing ownership bundle when no waiter receives it.
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
    /// Builds one actor and its registry handle without holding the registry state lock or spawning a Tokio task.
    fn prepare_registration(
        &self,
        id: TaskId,
        label: Arc<str>,
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
            Arc::clone(&label),
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
                label: Arc::clone(&label),
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
            label,
            join,
            cancel: task_token,
            done,
            completion,
            scheduled,
            cleanup,
            activity,
        }
    }

    /// Checks the complete static batch and registers all entries or none.
    ///
    /// Accepted actors wait behind one start gate.
    /// The gate opens only after every entry is indexed, all `TaskAdded` publications are attempted, and the direct batch reply send is attempted.
    /// A rejected batch inserts no entries and starts no task bodies.
    pub(super) async fn spawn_and_register_batch(
        &self,
        items: Vec<AddBatchItem>,
        reply: oneshot::Sender<AddReply>,
    ) {
        // Validate before resolving specs or constructing actors. The registry listener serializes
        // admissions; the second check below keeps insertion atomic with shutdown-side mutation.
        let reaper = self.actors.attempt_reaper();
        let mut seen = HashSet::with_capacity(items.len());
        let mut conflicting_ids = HashSet::new();
        let mut first_conflict = None;
        let current = {
            let st = self.state.read().await;
            let reaper_conflicts =
                reaper.reserves_labels(items.iter().map(|item| item.label.as_ref()));
            for (item, reaper_conflict) in items.iter().zip(reaper_conflicts) {
                let conflicts_with_registry =
                    st.by_label.contains_key(&item.label) || reaper_conflict;
                let repeats_in_batch = !seen.insert(item.label.as_ref());
                if conflicts_with_registry || repeats_in_batch {
                    first_conflict.get_or_insert_with(|| Arc::clone(&item.label));
                    conflicting_ids.insert(item.id);
                }
            }
            st.tasks.len()
        };

        if let Some(name) = first_conflict {
            for item in &items {
                let conflict = conflicting_ids.contains(&item.id);
                let (kind, reason) = if conflict {
                    (RejectionKind::AlreadyExists, reasons::ALREADY_EXISTS)
                } else {
                    (RejectionKind::BatchRejected, reasons::BATCH_REJECTED)
                };
                self.bus.publish_lazy(|| {
                    Event::new(EventKind::TaskAddFailed)
                        .with_task(Arc::clone(&item.label))
                        .with_id(item.id)
                        .with_rejection_kind(kind)
                        .with_reason(reason)
                });
            }
            let _ = reply.send(Err(RuntimeError::TaskAlreadyExists { name }));
            drop(items);
            return;
        }

        if let Some(limit) = self.registered_limit_exceeded(current, items.len()) {
            let reason = format!("{}: {current}/{limit}", reasons::REGISTERED_TASK_LIMIT);
            for item in &items {
                self.bus.publish_lazy(|| {
                    Event::new(EventKind::TaskAddFailed)
                        .with_task(Arc::clone(&item.label))
                        .with_id(item.id)
                        .with_rejection_kind(RejectionKind::ResourceLimit)
                        .with_reason(reason.clone())
                });
            }
            let _ = reply.send(Err(RuntimeError::ResourceLimitReached {
                resource: "registered_tasks",
                limit,
            }));
            drop(items);
            return;
        }

        let (start_tx, start_rx) = watch::channel(false);
        let prepared: Vec<_> = items
            .into_iter()
            .map(|item| {
                self.prepare_registration(
                    item.id,
                    item.label,
                    item.owned,
                    None,
                    None,
                    start_rx.clone(),
                )
            })
            .collect();

        let mut st = self.state.write().await;
        let mut seen = HashSet::with_capacity(prepared.len());
        let mut conflicting_ids = HashSet::new();
        let mut first_conflict = None;

        let reaper_conflicts =
            reaper.reserves_labels(prepared.iter().map(|item| item.label.as_ref()));
        for (item, reaper_conflict) in prepared.iter().zip(reaper_conflicts) {
            let conflicts_with_registry = st.by_label.contains_key(&item.label) || reaper_conflict;
            let repeats_in_batch = !seen.insert(item.label.as_ref());
            if conflicts_with_registry || repeats_in_batch {
                first_conflict.get_or_insert_with(|| Arc::clone(&item.label));
                conflicting_ids.insert(item.id);
            }
        }

        if let Some(name) = first_conflict {
            drop(st);
            for item in prepared {
                let reason = if conflicting_ids.contains(&item.id) {
                    reasons::ALREADY_EXISTS
                } else {
                    reasons::BATCH_REJECTED
                };
                let rejection_kind = if conflicting_ids.contains(&item.id) {
                    RejectionKind::AlreadyExists
                } else {
                    RejectionKind::BatchRejected
                };
                self.bus.publish_lazy(|| {
                    Event::new(EventKind::TaskAddFailed)
                        .with_task(Arc::clone(&item.label))
                        .with_id(item.id)
                        .with_rejection_kind(rejection_kind)
                        .with_reason(reason)
                });
            }
            let _ = reply.send(Err(RuntimeError::TaskAlreadyExists { name }));
            return;
        }

        if let Some(limit) = self.registered_limit_exceeded(st.tasks.len(), prepared.len()) {
            let current = st.tasks.len();
            drop(st);
            let reason = format!("{}: {current}/{limit}", reasons::REGISTERED_TASK_LIMIT);
            for item in &prepared {
                self.bus.publish_lazy(|| {
                    Event::new(EventKind::TaskAddFailed)
                        .with_task(Arc::clone(&item.label))
                        .with_id(item.id)
                        .with_rejection_kind(RejectionKind::ResourceLimit)
                        .with_reason(reason.clone())
                });
            }
            let _ = reply.send(Err(RuntimeError::ResourceLimitReached {
                resource: "registered_tasks",
                limit,
            }));
            drop(prepared);
            return;
        }

        let mut accepted = Vec::with_capacity(prepared.len());
        for item in prepared {
            let PreparedRegistration {
                id,
                label,
                join,
                cancel,
                done,
                completion,
                scheduled,
                cleanup,
                activity,
            } = item;
            let entry = Entry {
                label: Arc::clone(&label),
                activity,
                state: EntryState::Registered(Box::new(Handle::new(
                    join,
                    cancel,
                    done,
                    completion.clone(),
                    HandleCleanup::new(id, self.actors.attempt_reaper(), completion, cleanup),
                ))),
            };
            st.tasks.insert(id, entry);
            st.by_label.insert(Arc::clone(&label), id);
            accepted.push((id, label, scheduled));
        }
        drop(st);

        for (id, label, _) in &accepted {
            self.bus.publish_lazy(|| {
                Event::new(EventKind::TaskAdded)
                    .with_task(Arc::clone(label))
                    .with_id(*id)
            });
        }
        let _ = reply.send(Ok(()));
        self.actors
            .schedule_batch(accepted.into_iter().map(|(_, _, scheduled)| scheduled));
        start_tx.send_replace(true);
    }

    /// Prepares, registers, and schedules an actor under `id`.
    ///
    /// Duplicate task names are rejected.
    /// An accepted actor starts only after its entry is indexed, `TaskAdded` is published, and the direct reply send is attempted.
    ///
    /// Direct `add_and_watch` callers still receive [`RuntimeError::TaskAlreadyExists`] because registration confirmation fails before the waiter is returned.
    pub(super) async fn spawn_and_register(
        &self,
        id: TaskId,
        label: Arc<str>,
        owned: OwnedTask<TaskSpec>,
        done: Option<OutcomeTx>,
        completion: Option<RemovalCompletion>,
        reply: oneshot::Sender<AddReply>,
    ) {
        let reaper = self.actors.attempt_reaper();
        let validation = {
            let st = self.state.read().await;
            if st.by_label.contains_key(&label) || reaper.reserves_label(&label) {
                Err(RuntimeError::TaskAlreadyExists {
                    name: Arc::clone(&label),
                })
            } else if let Some(limit) = self.registered_limit_exceeded(st.tasks.len(), 1) {
                Err(RuntimeError::ResourceLimitReached {
                    resource: "registered_tasks",
                    limit,
                })
            } else {
                Ok(())
            }
        };
        if let Err(error) = validation {
            let (kind, reason) = match &error {
                RuntimeError::TaskAlreadyExists { .. } => (
                    RejectionKind::AlreadyExists,
                    reasons::ALREADY_EXISTS.to_owned(),
                ),
                RuntimeError::ResourceLimitReached { limit, .. } => (
                    RejectionKind::ResourceLimit,
                    format!("{}: {limit}", reasons::REGISTERED_TASK_LIMIT),
                ),
                _ => unreachable!("single admission validation returns duplicate or limit"),
            };
            let _ = reply.send(Err(error));
            let outcome = TaskOutcome::Rejected {
                kind,
                reason: Arc::from(reason.as_str()),
            };
            let (spec, mut cleanup) = owned.into_parts();
            deliver_or_attach_rejection(done, outcome, &mut cleanup);
            drop(spec);
            cleanup.submit();
            self.bus.publish_lazy(|| {
                Event::new(EventKind::TaskAddFailed)
                    .with_task(Arc::clone(&label))
                    .with_id(id)
                    .with_rejection_kind(kind)
                    .with_reason(reason)
            });
            return;
        }

        let (start_tx, start_rx) = watch::channel(false);
        let mut prepared = self.prepare_registration(id, label, owned, done, completion, start_rx);

        let mut st = self.state.write().await;
        if st.by_label.contains_key(&prepared.label) || reaper.reserves_label(&prepared.label) {
            drop(st);
            let _ = reply.send(Err(RuntimeError::TaskAlreadyExists {
                name: Arc::clone(&prepared.label),
            }));
            let outcome = TaskOutcome::Rejected {
                kind: RejectionKind::AlreadyExists,
                reason: Arc::from(reasons::ALREADY_EXISTS),
            };
            deliver_or_attach_rejection(prepared.done.take(), outcome, &mut prepared.cleanup);
            self.bus.publish_lazy(|| {
                Event::new(EventKind::TaskAddFailed)
                    .with_task(Arc::clone(&prepared.label))
                    .with_id(id)
                    .with_rejection_kind(RejectionKind::AlreadyExists)
                    .with_reason(reasons::ALREADY_EXISTS)
            });
            return;
        }

        if let Some(limit) = self.registered_limit_exceeded(st.tasks.len(), 1) {
            drop(st);
            let reason = format!("{}: {limit}", reasons::REGISTERED_TASK_LIMIT);
            let _ = reply.send(Err(RuntimeError::ResourceLimitReached {
                resource: "registered_tasks",
                limit,
            }));
            let outcome = TaskOutcome::Rejected {
                kind: RejectionKind::ResourceLimit,
                reason: Arc::from(reason.as_str()),
            };
            deliver_or_attach_rejection(prepared.done.take(), outcome, &mut prepared.cleanup);
            self.bus.publish_lazy(|| {
                Event::new(EventKind::TaskAddFailed)
                    .with_task(Arc::clone(&prepared.label))
                    .with_id(id)
                    .with_rejection_kind(RejectionKind::ResourceLimit)
                    .with_reason(reason)
            });
            return;
        }

        let PreparedRegistration {
            id,
            label,
            join,
            cancel,
            done,
            completion,
            scheduled,
            cleanup,
            activity,
        } = prepared;
        let entry = Entry {
            label: Arc::clone(&label),
            activity,
            state: EntryState::Registered(Box::new(Handle::new(
                join,
                cancel,
                done,
                completion.clone(),
                HandleCleanup::new(id, self.actors.attempt_reaper(), completion, cleanup),
            ))),
        };
        st.tasks.insert(id, entry);
        st.by_label.insert(label.clone(), id);
        drop(st);

        self.bus.publish_lazy(|| {
            Event::new(EventKind::TaskAdded)
                .with_task(Arc::clone(&label))
                .with_id(id)
        });
        let _ = reply.send(Ok(()));
        self.actors.schedule(scheduled);
        start_tx.send_replace(true);
    }

    /// Returns the configured physical-registration budget when `incoming` would exceed it.
    ///
    /// Reaping attempts are intentionally charged even during the short handoff window in which
    /// their registry entry may still exist. This conservative overlap keeps the resource bound
    /// strict without coupling the registry lock to the reaper lock.
    fn registered_limit_exceeded(&self, current: usize, incoming: usize) -> Option<usize> {
        let limit = self.max_registered_tasks?.get();
        let exceeds = current
            .checked_add(self.actors.reaping_attempts())
            .and_then(|used| used.checked_add(incoming))
            .is_none_or(|total| total > limit);
        exceeds.then_some(limit)
    }
}
