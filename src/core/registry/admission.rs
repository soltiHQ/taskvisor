//! Actor preparation, scheduling, and atomic single/batch admission.

use std::{collections::HashSet, sync::Arc};

use tokio::sync::{oneshot, watch};

use super::{
    Registry,
    completion::{OutcomeTx, RemovalCompletion},
    protocol::{AddBatchItem, AddReply},
    scheduler::{ActorHandle, ScheduledActor},
    state::{Entry, EntryState, Handle},
};
use crate::{
    core::actor::{ActorExitReason, TaskActor, TaskActorParams},
    core::outcome::TaskOutcome,
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
}

impl Registry {
    /// Builds one actor and its registry handle without holding the registry state lock or spawning a Tokio task.
    fn prepare_registration(
        &self,
        id: TaskId,
        label: Arc<str>,
        spec: TaskSpec,
        done: Option<OutcomeTx>,
        completion: Option<RemovalCompletion>,
        mut start: watch::Receiver<bool>,
    ) -> PreparedRegistration {
        let task_token = self.runtime_token.child_token();
        let spec = spec.resolve(&self.task_defaults);

        let actor = TaskActor::new(
            self.bus.clone(),
            Arc::clone(&label),
            spec.task().clone(),
            TaskActorParams {
                restart: spec.restart(),
                backoff: spec.backoff(),
                timeout: spec.timeout(),
                max_retries: spec.max_retries(),
            },
            self.semaphore.clone(),
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
        let (scheduled, join) =
            ScheduledActor::new(id, self.listener.completion_tx.clone(), actor_future);

        PreparedRegistration {
            id,
            label,
            join,
            cancel: task_token,
            done,
            completion: completion.unwrap_or_else(RemovalCompletion::new),
            scheduled,
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
        let (start_tx, start_rx) = watch::channel(false);
        let prepared: Vec<_> = items
            .into_iter()
            .map(|item| {
                self.prepare_registration(
                    item.id,
                    item.label,
                    item.spec,
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

        for item in &prepared {
            let conflicts_with_registry = st.by_label.contains_key(&item.label);
            let repeats_in_batch = !seen.insert(Arc::clone(&item.label));
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
                self.bus.publish(
                    Event::new(EventKind::TaskAddFailed)
                        .with_task(item.label)
                        .with_id(item.id)
                        .with_rejection_kind(rejection_kind)
                        .with_reason(reason),
                );
            }
            let _ = reply.send(Err(RuntimeError::TaskAlreadyExists { name }));
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
            } = item;
            let entry = Entry {
                label: Arc::clone(&label),
                state: EntryState::Registered(Handle {
                    join,
                    cancel,
                    done,
                    completion,
                }),
            };
            st.tasks.insert(id, entry);
            st.by_label.insert(Arc::clone(&label), id);
            accepted.push((id, label, scheduled));
        }
        drop(st);

        for (id, label, _) in &accepted {
            self.bus.publish(
                Event::new(EventKind::TaskAdded)
                    .with_task(Arc::clone(label))
                    .with_id(*id),
            );
        }
        let _ = reply.send(Ok(()));
        self.scheduler
            .schedule_batch(
                accepted
                    .into_iter()
                    .map(|(_, _, scheduled)| scheduled)
                    .collect(),
            )
            .await;
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
        spec: TaskSpec,
        done: Option<OutcomeTx>,
        completion: Option<RemovalCompletion>,
        reply: oneshot::Sender<AddReply>,
    ) {
        let (start_tx, start_rx) = watch::channel(false);
        let prepared = self.prepare_registration(id, label, spec, done, completion, start_rx);

        let mut st = self.state.write().await;
        if st.by_label.contains_key(&prepared.label) {
            drop(st);
            let _ = reply.send(Err(RuntimeError::TaskAlreadyExists {
                name: Arc::clone(&prepared.label),
            }));
            if let Some(done) = prepared.done {
                let _ = done.send(TaskOutcome::Rejected {
                    kind: RejectionKind::AlreadyExists,
                    reason: Arc::from(reasons::ALREADY_EXISTS),
                });
            }
            self.bus.publish(
                Event::new(EventKind::TaskAddFailed)
                    .with_task(prepared.label)
                    .with_id(id)
                    .with_rejection_kind(RejectionKind::AlreadyExists)
                    .with_reason(reasons::ALREADY_EXISTS),
            );
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
        } = prepared;
        let entry = Entry {
            label: Arc::clone(&label),
            state: EntryState::Registered(Handle {
                join,
                cancel,
                done,
                completion,
            }),
        };
        st.tasks.insert(id, entry);
        st.by_label.insert(label.clone(), id);
        drop(st);

        self.bus.publish(
            Event::new(EventKind::TaskAdded)
                .with_task(label)
                .with_id(id),
        );
        let _ = reply.send(Ok(()));
        self.scheduler.schedule(scheduled).await;
        start_tx.send_replace(true);
    }
}
