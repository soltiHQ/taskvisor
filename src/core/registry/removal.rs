//! Removal arbitration, actor joins, and terminal cleanup.

use std::{sync::Arc, time::Duration};

use tokio::{
    sync::{Notify, RwLock, oneshot},
    task::{JoinError, JoinHandle},
};

use super::{
    Registry,
    completion::{OutcomeTx, RemovalCompletion},
    protocol::{CancelDecision, CancelReply, RemoveReply},
    state::{EntryState, Handle, Inner, PendingJoins},
};
use crate::{
    core::{actor::ActorExitReason, outcome::TaskOutcome},
    events::{Bus, Event, EventKind},
    identity::TaskId,
};

/// Terminal result passed from the single join owner to registry cleanup.
pub(super) enum JoinCompletion {
    Joined(Result<ActorExitReason, JoinError>),
    ForceAborted,
}

/// Data needed to commit one actor's terminal registry cleanup.
pub(super) struct RemovalReport {
    pub(super) id: TaskId,
    pub(super) outcome: Option<OutcomeTx>,
    pub(super) join: JoinCompletion,
    pub(super) completion: RemovalCompletion,
}

/// Registry-side work selected for one cancel command.
struct CancelAction {
    decision: CancelDecision,
    handle: Option<Handle>,
}

impl Registry {
    /// Waits up to `grace` for detached join reporters.
    ///
    /// A reporter decrements the pending count only after it removes registry membership, attempts
    /// to send the optional watched outcome, and attempts the final `TaskRemoved` publication.
    /// It completes the shared `RemovalCompletion` just after that decrement, once the registry state lock is released.
    /// Therefore, this pending-join barrier is not itself a cancellation-completion barrier.
    ///
    /// Returns labels for reporters still in flight when `grace` expires.
    pub async fn wait_joins_within(&self, grace: Duration) -> Vec<Arc<str>> {
        let _ = tokio::time::timeout(grace, self.pending_joins.wait_drained()).await;
        self.pending_joins.pending_labels()
    }

    /// Claims and cancels every entry still in `Registered`, then joins those actors within one shared grace window.
    ///
    /// Entries already in `Removing` keep their existing join owner.
    /// This method waits for all pending reporters only until the same deadline.
    ///
    /// Returns labels of actors claimed here that had to be force-aborted.
    /// [`wait_joins_within`](Self::wait_joins_within) reports older join reporters that remain in flight.
    pub async fn cancel_all_within(&self, grace: Duration) -> Vec<Arc<str>> {
        let grace = grace.min(Duration::from_secs(60 * 60 * 24 * 365 * 30));
        let handles: Vec<(TaskId, Arc<str>, Handle, RemovalCompletion)> = {
            let mut st = self.state.write().await;
            let ids: Vec<TaskId> = st.tasks.keys().copied().collect();
            ids.into_iter()
                .filter_map(|id| {
                    Self::claim_registered(&mut st, &self.pending_joins, id)
                        .map(|(label, handle, completion)| (id, label, handle, completion))
                })
                .collect()
        };
        for (_, _, h, _) in &handles {
            h.cancel.cancel();
        }

        let deadline = tokio::time::Instant::now() + grace;
        let mut stuck = Vec::new();

        for (id, label, h, removal_completion) in handles {
            let mut join = h.join;
            match tokio::time::timeout_at(deadline, &mut join).await {
                Ok(res) => {
                    Self::finish_removal(
                        &self.state,
                        &self.empty_notify,
                        &self.pending_joins,
                        &self.bus,
                        RemovalReport {
                            id,
                            outcome: h.done,
                            join: JoinCompletion::Joined(res),
                            completion: removal_completion,
                        },
                    )
                    .await;
                }
                Err(_elapsed) => {
                    join.abort();
                    let _ = join.await;
                    stuck.push(Arc::clone(&label));
                    Self::finish_removal(
                        &self.state,
                        &self.empty_notify,
                        &self.pending_joins,
                        &self.bus,
                        RemovalReport {
                            id,
                            outcome: h.done,
                            join: JoinCompletion::ForceAborted,
                            completion: removal_completion,
                        },
                    )
                    .await;
                }
            }
        }
        let _ = tokio::time::timeout_at(deadline, self.pending_joins.wait_drained()).await;
        stuck
    }

    /// Removes a task by identity.
    ///
    /// `Ok(true)` means this command claimed the actor and triggered cancellation.
    /// Membership remains until terminal join cleanup.
    ///
    /// `Ok(false)` means the entry is unknown or another cleanup owner already claimed it.
    /// This command does not create a second join owner or duplicate terminal event; an existing owner can still publish `TaskRemoved` later.
    pub(super) async fn remove_task(&self, id: TaskId, reply: oneshot::Sender<RemoveReply>) {
        if let Some((_label, handle, completion)) = self.claim_task(id).await {
            handle.cancel.cancel();
            let _ = reply.send(Ok(true));
            self.spawn_join_report(id, handle.join, Some(self.grace), handle.done, completion);
        } else {
            let _ = reply.send(Ok(false));
        }
    }

    /// Resolves one label and claims its current owner under the same state lock.
    ///
    /// A missing label returns `Ok(false)` without a request event.
    /// An entry already in `Removing` gets another request event but also returns `Ok(false)`.
    pub(super) async fn remove_task_by_label(
        &self,
        label: Arc<str>,
        reply: oneshot::Sender<RemoveReply>,
    ) {
        let claimed = {
            let mut st = self.state.write().await;
            let Some(id) = st.by_label.get(label.as_ref()).copied() else {
                drop(st);
                let _ = reply.send(Ok(false));
                return;
            };

            self.bus.publish(
                Event::new(EventKind::TaskRemoveRequested)
                    .with_task(Arc::clone(&label))
                    .with_id(id),
            );
            Self::claim_registered(&mut st, &self.pending_joins, id)
                .map(|(_entry_label, handle, completion)| (id, handle, completion))
        };

        if let Some((id, handle, completion)) = claimed {
            handle.cancel.cancel();
            let _ = reply.send(Ok(true));
            self.spawn_join_report(id, handle.join, Some(self.grace), handle.done, completion);
        } else {
            let _ = reply.send(Ok(false));
        }
    }

    /// Claims or joins cancellation by identity and returns a shared terminal decision.
    pub(super) async fn cancel_task(&self, id: TaskId, reply: oneshot::Sender<CancelReply>) {
        let action = {
            let mut st = self.state.write().await;
            if !st.tasks.contains_key(&id) {
                None
            } else {
                self.bus.publish(
                    Event::new(EventKind::TaskRemoveRequested)
                        .with_id(id)
                        .with_reason("manual_cancel"),
                );
                Self::cancel_action(&mut st, &self.pending_joins, id)
            }
        };
        self.resolve_cancel_action(action, reply);
    }

    /// Resolves a label and claims or joins cancellation under the same state lock.
    pub(super) async fn cancel_task_by_label(
        &self,
        label: Arc<str>,
        reply: oneshot::Sender<CancelReply>,
    ) {
        let action = {
            let mut st = self.state.write().await;
            let Some(id) = st.by_label.get(label.as_ref()).copied() else {
                drop(st);
                let _ = reply.send(Ok(None));
                return;
            };

            self.bus.publish(
                Event::new(EventKind::TaskRemoveRequested)
                    .with_task(label)
                    .with_id(id)
                    .with_reason("manual_cancel"),
            );
            Self::cancel_action(&mut st, &self.pending_joins, id)
        };
        self.resolve_cancel_action(action, reply);
    }

    /// Selects one cancel action while registry state is locked.
    fn cancel_action(
        st: &mut Inner,
        pending_joins: &PendingJoins,
        id: TaskId,
    ) -> Option<CancelAction> {
        let existing_completion = {
            let entry = st.tasks.get(&id)?;
            match &entry.state {
                EntryState::Registered(_) => None,
                EntryState::Removing { completion } => Some(completion.clone()),
            }
        };
        if let Some(completion) = existing_completion {
            return Some(CancelAction {
                decision: CancelDecision {
                    id,
                    claimed: false,
                    completion,
                },
                handle: None,
            });
        }

        let (_label, handle, completion) = Self::claim_registered(st, pending_joins, id)
            .expect("a registered entry must be claimable while state is locked");
        Some(CancelAction {
            decision: CancelDecision {
                id,
                claimed: true,
                completion,
            },
            handle: Some(handle),
        })
    }

    /// Sends one cancel decision and starts the join owner when this command claimed it.
    fn resolve_cancel_action(
        &self,
        action: Option<CancelAction>,
        reply: oneshot::Sender<CancelReply>,
    ) {
        let Some(CancelAction { decision, handle }) = action else {
            let _ = reply.send(Ok(None));
            return;
        };

        if let Some(handle) = handle {
            handle.cancel.cancel();
            let completion = decision.completion.clone();
            let id = decision.id;
            let _ = reply.send(Ok(Some(decision)));
            self.spawn_join_report(id, handle.join, Some(self.grace), handle.done, completion);
        } else {
            let _ = reply.send(Ok(Some(decision)));
        }
    }

    /// Cleans up a finished actor by identity.
    ///
    /// Called after the actor's reliable completion signal is received.
    /// Duplicate or stale completion signals are no-ops.
    pub(super) async fn cleanup_task(&self, id: TaskId) {
        if let Some((_label, handle, completion)) = self.claim_task(id).await {
            self.spawn_join_report(id, handle.join, Some(self.grace), handle.done, completion);
        }
    }

    /// Changes one task from `Registered` to `Removing`.
    ///
    /// The winning caller gets the only actor handle.
    /// Identity and label indexes stay in the registry until that caller finishes the join.
    async fn claim_task(&self, id: TaskId) -> Option<(Arc<str>, Handle, RemovalCompletion)> {
        let mut st = self.state.write().await;
        Self::claim_registered(&mut st, &self.pending_joins, id)
    }

    /// Locked implementation of the `Registered` to `Removing` transition.
    fn claim_registered(
        st: &mut Inner,
        pending_joins: &PendingJoins,
        id: TaskId,
    ) -> Option<(Arc<str>, Handle, RemovalCompletion)> {
        let entry = st.tasks.get_mut(&id)?;
        if matches!(&entry.state, EntryState::Removing { .. }) {
            return None;
        }

        let completion = match &entry.state {
            EntryState::Registered(handle) => handle.completion.clone(),
            EntryState::Removing { .. } => unreachable!("a removing entry was checked above"),
        };
        let EntryState::Registered(handle) = std::mem::replace(
            &mut entry.state,
            EntryState::Removing {
                completion: completion.clone(),
            },
        ) else {
            unreachable!("a removing entry was checked above")
        };
        let label = Arc::clone(&entry.label);
        pending_joins.inc(id);
        pending_joins.label(id, Arc::clone(&label));
        Some((label, handle, completion))
    }

    /// Joins an actor in a detached task and commits its final result.
    ///
    /// If `force_after` is `Some`, the join is bounded by that duration.
    /// An actor that misses the deadline is aborted and a watched task resolves to [`TaskOutcome::ForceAborted`].
    ///
    /// Both normal join and force-abort paths remove membership, resolve the optional outcome, and publish one final `TaskRemoved`.
    fn spawn_join_report(
        &self,
        id: TaskId,
        join: JoinHandle<ActorExitReason>,
        force_after: Option<Duration>,
        done: Option<OutcomeTx>,
        removal_completion: RemovalCompletion,
    ) {
        let bus = self.bus.clone();
        let state = Arc::clone(&self.state);
        let empty_notify = Arc::clone(&self.empty_notify);
        let pending = Arc::clone(&self.pending_joins);
        tokio::spawn(async move {
            let mut join = join;
            let completion = match force_after {
                Some(grace) => match tokio::time::timeout(grace, &mut join).await {
                    Ok(res) => JoinCompletion::Joined(res),
                    Err(_) => {
                        join.abort();
                        let _ = join.await;
                        JoinCompletion::ForceAborted
                    }
                },
                None => JoinCompletion::Joined(join.await),
            };

            Self::finish_removal(
                &state,
                &empty_notify,
                &pending,
                &bus,
                RemovalReport {
                    id,
                    outcome: done,
                    join: completion,
                    completion: removal_completion,
                },
            )
            .await;
        });
    }

    /// Commits terminal cleanup for one `Removing` entry.
    ///
    /// State removal, outcome delivery, terminal events, and pending-join cleanup finish before an empty-registry waiter can continue.
    pub(super) async fn finish_removal(
        state: &RwLock<Inner>,
        empty_notify: &Notify,
        pending_joins: &PendingJoins,
        bus: &Bus,
        report: RemovalReport,
    ) {
        let RemovalReport {
            id,
            outcome,
            join,
            completion: removal_completion,
        } = report;
        let mut st = state.write().await;
        let is_removing = st
            .tasks
            .get(&id)
            .is_some_and(|entry| matches!(&entry.state, EntryState::Removing { .. }));
        if !is_removing {
            drop(st);
            pending_joins.dec(id);
            removal_completion.complete();
            return;
        }

        let entry = st
            .tasks
            .remove(&id)
            .expect("the removing entry was checked above");
        let EntryState::Removing {
            completion: state_completion,
        } = entry.state
        else {
            unreachable!("the removing entry was checked above")
        };
        if st.by_label.get(entry.label.as_ref()) == Some(&id) {
            st.by_label.remove(entry.label.as_ref());
        }

        match join {
            JoinCompletion::Joined(res) => {
                Self::report_join(bus, id, &entry.label, res, outcome);
            }
            JoinCompletion::ForceAborted => {
                if let Some(done) = outcome {
                    let _ = done.send(TaskOutcome::ForceAborted);
                }
                bus.publish(
                    Event::new(EventKind::TaskRemoved)
                        .with_task(Arc::clone(&entry.label))
                        .with_id(id)
                        .with_reason("force_terminated_after_grace"),
                );
            }
        }
        pending_joins.dec(id);
        let is_empty = st.tasks.is_empty();
        drop(st);

        // Wake completion waiters only after id and label membership is removed and the
        // registry state lock is released. A controller may safely admit the next task now.
        state_completion.complete();
        removal_completion.complete();
        if is_empty {
            empty_notify.notify_waiters();
        }
    }

    /// Reports the result of a joined actor.
    ///
    /// Sends the watched [`TaskOutcome`] if present, publishes `ActorDead` for an actor panic, and always publishes `TaskRemoved` for this joined actor.
    fn report_join(
        bus: &Bus,
        id: TaskId,
        name: &str,
        res: Result<ActorExitReason, JoinError>,
        done: Option<OutcomeTx>,
    ) {
        if let Err(e) = &res
            && e.is_panic()
        {
            bus.publish(
                Event::new(EventKind::ActorDead)
                    .with_task(name)
                    .with_id(id)
                    .with_reason("actor_panic"),
            );
        }
        if let Some(done) = done {
            let _ = done.send(Self::outcome_of(res));
        }
        bus.publish(
            Event::new(EventKind::TaskRemoved)
                .with_task(name)
                .with_id(id),
        );
    }

    /// Maps a joined actor result to the public [`TaskOutcome`].
    fn outcome_of(res: Result<ActorExitReason, JoinError>) -> TaskOutcome {
        match res {
            Ok(ActorExitReason::Completed) => TaskOutcome::Completed,
            Ok(ActorExitReason::Canceled) => TaskOutcome::Canceled,
            Ok(ActorExitReason::Exhausted {
                reason,
                exit_code,
                source,
            }) => TaskOutcome::Failed {
                reason,
                exit_code,
                source,
            },
            Ok(ActorExitReason::Fatal {
                reason,
                exit_code,
                source,
            }) => TaskOutcome::Fatal {
                reason,
                exit_code,
                source,
            },
            Err(e) if e.is_panic() => TaskOutcome::Panicked,
            Err(_aborted) => TaskOutcome::ForceAborted,
        }
    }
}
