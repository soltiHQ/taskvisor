//! Actor construction and atomic single/batch admission.

use std::{collections::HashSet, future::Future, sync::Arc};

use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};

use super::{
    Registry,
    completion::{OutcomeTx, RemovalCompletion},
    protocol::{AddBatchItem, AddReply},
    state::{Entry, EntryState, Handle},
};
use crate::{
    core::actor::{ActorExitReason, TaskActor, TaskActorParams},
    core::outcome::TaskOutcome,
    error::RuntimeError,
    events::{Event, EventKind},
    identity::TaskId,
    reasons,
    tasks::TaskSpec,
};

/// Sends one completion signal when an actor task returns, panics, or is aborted.
struct ActorCompletionGuard {
    id: TaskId,
    tx: mpsc::UnboundedSender<TaskId>,
}

impl Drop for ActorCompletionGuard {
    fn drop(&mut self) {
        let _ = self.tx.send(self.id);
    }
}

impl Registry {
    pub(super) fn spawn_tracked_actor(
        id: TaskId,
        completion_tx: mpsc::UnboundedSender<TaskId>,
        future: impl Future<Output = ActorExitReason> + Send + 'static,
    ) -> JoinHandle<ActorExitReason> {
        let completion = ActorCompletionGuard {
            id,
            tx: completion_tx,
        };
        tokio::spawn(async move {
            let _completion = completion;
            future.await
        })
    }

    /// Spawns one registered actor, optionally held behind a batch start gate.
    fn spawn_entry(
        &self,
        id: TaskId,
        label: Arc<str>,
        spec: TaskSpec,
        done: Option<OutcomeTx>,
        completion: Option<RemovalCompletion>,
        start: Option<watch::Receiver<bool>>,
    ) -> Entry {
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
            if let Some(mut start) = start {
                loop {
                    if *start.borrow_and_update() {
                        break;
                    }
                    if start.changed().await.is_err() {
                        return ActorExitReason::Canceled;
                    }
                }
            }
            actor.run(task_token_clone).await
        };
        let join_handle =
            Self::spawn_tracked_actor(id, self.listener.completion_tx.clone(), actor_future);

        Entry {
            label,
            state: EntryState::Registered(Handle {
                join: join_handle,
                cancel: task_token,
                done,
                completion: completion.unwrap_or_else(RemovalCompletion::new),
            }),
        }
    }

    /// Validates and registers a complete static task batch without partial start.
    pub(super) async fn spawn_and_register_batch(
        &self,
        items: Vec<AddBatchItem>,
        reply: oneshot::Sender<AddReply>,
    ) {
        let mut st = self.state.write().await;
        let mut seen = HashSet::with_capacity(items.len());
        let mut conflicting_ids = HashSet::new();
        let mut first_conflict = None;

        for item in &items {
            let conflicts_with_registry = st.by_label.contains_key(&item.label);
            let repeats_in_batch = !seen.insert(Arc::clone(&item.label));
            if conflicts_with_registry || repeats_in_batch {
                first_conflict.get_or_insert_with(|| Arc::clone(&item.label));
                conflicting_ids.insert(item.id);
            }
        }

        if let Some(name) = first_conflict {
            drop(st);
            for item in items {
                let reason = if conflicting_ids.contains(&item.id) {
                    reasons::ALREADY_EXISTS
                } else {
                    reasons::BATCH_REJECTED
                };
                self.bus.publish(
                    Event::new(EventKind::TaskAddFailed)
                        .with_task(item.label)
                        .with_id(item.id)
                        .with_reason(reason),
                );
            }
            let _ = reply.send(Err(RuntimeError::TaskAlreadyExists { name }));
            return;
        }

        let (start_tx, start_rx) = watch::channel(false);
        let mut accepted = Vec::with_capacity(items.len());
        for item in items {
            let id = item.id;
            let label = item.label;
            let entry = self.spawn_entry(
                id,
                Arc::clone(&label),
                item.spec,
                None,
                None,
                Some(start_rx.clone()),
            );
            st.tasks.insert(id, entry);
            st.by_label.insert(Arc::clone(&label), id);
            accepted.push((id, label));
        }
        drop(st);

        for (id, label) in accepted {
            self.bus.publish(
                Event::new(EventKind::TaskAdded)
                    .with_task(label)
                    .with_id(id),
            );
        }
        let _ = reply.send(Ok(()));
        start_tx.send_replace(true);
    }

    /// Spawns an actor and registers it under `id`.
    ///
    /// Duplicate task names are rejected.
    ///
    /// Direct `add_and_watch` callers still receive [`RuntimeError::TaskAlreadyExists`] because registration confirmation fails before the waiter is returned.
    pub(super) async fn spawn_and_register(
        &self,
        id: TaskId,
        spec: TaskSpec,
        done: Option<OutcomeTx>,
        completion: Option<RemovalCompletion>,
        reply: oneshot::Sender<AddReply>,
    ) {
        let label: Arc<str> = Arc::from(spec.task().name());

        let mut st = self.state.write().await;
        if st.by_label.contains_key(&label) {
            drop(st);
            let _ = reply.send(Err(RuntimeError::TaskAlreadyExists {
                name: Arc::clone(&label),
            }));
            if let Some(done) = done {
                let _ = done.send(TaskOutcome::Rejected {
                    reason: Arc::from(reasons::ALREADY_EXISTS),
                });
            }
            self.bus.publish(
                Event::new(EventKind::TaskAddFailed)
                    .with_task(label)
                    .with_id(id)
                    .with_reason(reasons::ALREADY_EXISTS),
            );
            return;
        }

        let entry = self.spawn_entry(id, Arc::clone(&label), spec, done, completion, None);
        st.tasks.insert(id, entry);
        st.by_label.insert(label.clone(), id);
        drop(st);

        let _ = reply.send(Ok(()));
        self.bus.publish(
            Event::new(EventKind::TaskAdded)
                .with_task(label)
                .with_id(id),
        );
    }
}
