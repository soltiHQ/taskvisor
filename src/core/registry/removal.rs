//! Removal arbitration, actor joins, and terminal cleanup.

use std::{sync::Arc, time::Duration};

use tokio::sync::{Notify, RwLock, oneshot};

use super::{
    Registry,
    completion::{OutcomeTx, RemovalCompletion},
    protocol::{CancelDecision, CancelReply, RemoveReply},
    scheduler::{ActorJoinError, AttemptReaper},
    state::{EntryState, Handle, Inner, PendingJoins},
};
use crate::{
    core::{actor::ActorExitReason, deferred_drop::DropBundle, outcome::TaskOutcome},
    events::{Bus, Event, EventKind},
    identity::TaskId,
};

/// Terminal result passed from the single join owner to registry cleanup.
pub(super) enum JoinCompletion {
    Joined(Result<ActorExitReason, ActorJoinError>),
    ForceAborted,
}

/// Data needed to commit one actor's terminal registry cleanup.
pub(super) struct RemovalReport {
    pub(super) id: TaskId,
    pub(super) outcome: Option<OutcomeTx>,
    pub(super) join: JoinCompletion,
    pub(super) completion: RemovalCompletion,
    pub(super) cleanup: DropBundle,
}

/// Keeps an undelivered terminal outcome on the isolated destruction path even
/// if event publication or future reporting code unwinds before channel send.
struct OutcomeDropGuard<'a> {
    outcome: Option<TaskOutcome>,
    cleanup: &'a mut DropBundle,
}

impl<'a> OutcomeDropGuard<'a> {
    fn new(outcome: TaskOutcome, cleanup: &'a mut DropBundle) -> Self {
        Self {
            outcome: Some(outcome),
            cleanup,
        }
    }

    fn get(&self) -> &TaskOutcome {
        self.outcome
            .as_ref()
            .expect("the terminal outcome remains guarded until delivery")
    }

    fn take(&mut self) -> TaskOutcome {
        self.outcome
            .take()
            .expect("the terminal outcome is delivered at most once")
    }
}

impl Drop for OutcomeDropGuard<'_> {
    fn drop(&mut self) {
        if let Some(outcome) = self.outcome.take() {
            self.cleanup.attach_outcome(outcome);
        }
    }
}

/// Owns every part of a terminal report until registry membership is removed.
///
/// If the reporting future is canceled while waiting for the registry lock,
/// this guard transfers the whole commit to one detached continuation. Logical
/// completion and the pending-join barrier therefore remain tied to membership
/// removal instead of being released by cancellation.
struct PendingTerminalReport {
    state: Arc<RwLock<Inner>>,
    empty_notify: Arc<Notify>,
    pending_joins: Arc<PendingJoins>,
    bus: Bus,
    id: TaskId,
    outcome: Option<TaskOutcome>,
    done: Option<OutcomeTx>,
    cleanup: Option<DropBundle>,
    reaper: AttemptReaper,
    completion: RemovalCompletion,
    detach_on_drop: bool,
}

impl PendingTerminalReport {
    fn take(&mut self) -> (TaskOutcome, Option<OutcomeTx>, DropBundle) {
        (
            self.outcome
                .take()
                .expect("one terminal outcome is classified"),
            self.done.take(),
            self.cleanup
                .take()
                .expect("one charged terminal bundle is retained"),
        )
    }

    /// Removes one authoritative membership entry, then commits all terminal
    /// reporting and completion side effects without another cancellation
    /// point.
    async fn commit(&mut self) {
        let removed = {
            let mut st = self.state.write().await;
            let is_removing = st
                .tasks
                .get(&self.id)
                .is_some_and(|entry| matches!(&entry.state, EntryState::Removing { .. }));
            if !is_removing {
                None
            } else {
                let entry = st
                    .tasks
                    .remove(&self.id)
                    .expect("the removing entry was checked above");
                let EntryState::Removing {
                    completion: state_completion,
                } = entry.state
                else {
                    unreachable!("the removing entry was checked above")
                };
                if st.by_label.get(entry.label.as_ref()) == Some(&self.id) {
                    st.by_label.remove(entry.label.as_ref());
                }
                let is_empty = st.tasks.is_empty();
                Some((entry.label, state_completion, is_empty))
            }
        };

        let Some((label, state_completion, is_empty)) = removed else {
            self.finish_without_membership();
            return;
        };

        let (terminal_outcome, outcome, cleanup) = self.take();
        let mut finalizer = TerminalFinalizer {
            id: self.id,
            empty_notify: &self.empty_notify,
            pending_joins: &self.pending_joins,
            state_completion: Some(state_completion),
            report_completion: self.completion.clone(),
            is_empty,
            terminal: Some((self.reaper.clone(), cleanup)),
        };
        let cleanup = &mut finalizer
            .terminal
            .as_mut()
            .expect("terminal ownership is installed")
            .1;

        Registry::report_outcome(
            &self.bus,
            self.id,
            &label,
            terminal_outcome,
            outcome,
            cleanup,
        );
        // Wake completion waiters only after id and label membership is removed,
        // reporting has been attempted, and the registry state lock is released.
        drop(finalizer);
    }

    /// Completes a stale duplicate report only after the state lock verified
    /// that no membership remains for it.
    fn finish_without_membership(&mut self) {
        if self.retain_terminal() {
            self.pending_joins.dec(self.id);
            self.completion.complete_logical();
        }
    }

    /// Transfers classified terminal ownership to the reaper without changing
    /// registry barriers or logical completion.
    fn retain_terminal(&mut self) -> bool {
        let Some(mut cleanup) = self.cleanup.take() else {
            return false;
        };
        if let Some(outcome) = self.outcome.take() {
            match self.done.take() {
                Some(done) => {
                    if let Err(undelivered) = done.send(outcome) {
                        cleanup.attach_outcome(undelivered);
                    }
                }
                None => cleanup.attach_outcome(outcome),
            }
        }
        self.reaper
            .attach_terminal(self.id, cleanup, None, self.completion.clone());
        true
    }

    /// Moves an interrupted commit into a guard whose cancellation fallback
    /// retains physical ownership without falsely completing logical removal.
    fn take_continuation(&mut self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            empty_notify: Arc::clone(&self.empty_notify),
            pending_joins: Arc::clone(&self.pending_joins),
            bus: self.bus.clone(),
            id: self.id,
            outcome: self.outcome.take(),
            done: self.done.take(),
            cleanup: self.cleanup.take(),
            reaper: self.reaper.clone(),
            completion: self.completion.clone(),
            detach_on_drop: false,
        }
    }

    /// Preserves the charged terminal ownership if the detached continuation
    /// itself is canceled by runtime teardown. Membership and logical latches
    /// deliberately remain incomplete because the state commit did not occur.
    fn retain_without_logical_completion(&mut self) {
        let _ = self.retain_terminal();
    }
}

impl Drop for PendingTerminalReport {
    fn drop(&mut self) {
        if self.cleanup.is_none() {
            return;
        }

        if self.detach_on_drop
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let mut continuation = self.take_continuation();
            drop(runtime.spawn(async move {
                continuation.commit().await;
            }));
            return;
        }

        self.retain_without_logical_completion();
    }
}

/// Registry-side work selected for one cancel command.
struct CancelAction {
    decision: CancelDecision,
    handle: Option<Handle>,
}

/// Commits the non-negotiable tail of one terminal registry transition.
///
/// Reporting a watched outcome can transfer user-owned error sources, and a
/// defensive future change could panic while publishing diagnostics. Keeping
/// this tail in a drop guard prevents either case from stranding registry
/// capacity or shutdown waiters.
pub(super) struct TerminalFinalizer<'a> {
    pub(super) id: TaskId,
    pub(super) empty_notify: &'a Notify,
    pub(super) pending_joins: &'a PendingJoins,
    pub(super) state_completion: Option<RemovalCompletion>,
    pub(super) report_completion: RemovalCompletion,
    pub(super) is_empty: bool,
    pub(super) terminal: Option<(AttemptReaper, DropBundle)>,
}

impl Drop for TerminalFinalizer<'_> {
    fn drop(&mut self) {
        if let Some((reaper, bundle)) = self.terminal.take() {
            reaper.attach_terminal(
                self.id,
                bundle,
                self.state_completion.clone(),
                self.report_completion.clone(),
            );
        }
        self.pending_joins.dec(self.id);
        if let Some(completion) = &self.state_completion {
            completion.complete_logical();
        }
        self.report_completion.complete_logical();
        if self.is_empty {
            self.empty_notify.notify_waiters();
        }
    }
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
        let reaper = self.actors.attempt_reaper();

        for (id, label, mut handle, removal_completion) in handles {
            let join = match tokio::time::timeout_at(deadline, handle.join_mut()).await {
                Ok(res) => JoinCompletion::Joined(res),
                Err(_elapsed) => {
                    handle.abort();
                    let _ = handle.join_mut().await;
                    stuck.push(Arc::clone(&label));
                    JoinCompletion::ForceAborted
                }
            };
            let (done, cleanup) = handle.into_report_parts();
            Self::finish_removal(
                &self.state,
                &self.empty_notify,
                &self.pending_joins,
                &self.bus,
                &reaper,
                RemovalReport {
                    id,
                    outcome: done,
                    join,
                    completion: removal_completion,
                    cleanup,
                },
            )
            .await;
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
            self.spawn_join_report(id, handle, Some(self.grace), completion);
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
        let resolved = {
            let mut st = self.state.write().await;
            st.by_label.get(label.as_ref()).copied().map(|id| {
                let claimed = Self::claim_registered(&mut st, &self.pending_joins, id)
                    .map(|(_entry_label, handle, completion)| (handle, completion));
                (id, claimed)
            })
        };
        let Some((id, claimed)) = resolved else {
            let _ = reply.send(Ok(false));
            return;
        };
        self.bus.publish_lazy(|| {
            Event::new(EventKind::TaskRemoveRequested)
                .with_task(Arc::clone(&label))
                .with_id(id)
        });

        if let Some((handle, completion)) = claimed {
            handle.cancel.cancel();
            let _ = reply.send(Ok(true));
            self.spawn_join_report(id, handle, Some(self.grace), completion);
        } else {
            let _ = reply.send(Ok(false));
        }
    }

    /// Claims or joins cancellation by identity and returns a shared terminal decision.
    pub(super) async fn cancel_task(&self, id: TaskId, reply: oneshot::Sender<CancelReply>) {
        let (found, action) = {
            let mut st = self.state.write().await;
            if !st.tasks.contains_key(&id) {
                (false, None)
            } else {
                (true, Self::cancel_action(&mut st, &self.pending_joins, id))
            }
        };
        if found {
            self.bus.publish_lazy(|| {
                Event::new(EventKind::TaskRemoveRequested)
                    .with_id(id)
                    .with_reason("manual_cancel")
            });
        }
        self.resolve_cancel_action(action, reply);
    }

    /// Resolves a label and claims or joins cancellation under the same state lock.
    pub(super) async fn cancel_task_by_label(
        &self,
        label: Arc<str>,
        reply: oneshot::Sender<CancelReply>,
    ) {
        let resolved = {
            let mut st = self.state.write().await;
            st.by_label.get(label.as_ref()).copied().map(|id| {
                let action = Self::cancel_action(&mut st, &self.pending_joins, id);
                (id, action)
            })
        };
        let Some((id, action)) = resolved else {
            let _ = reply.send(Ok(None));
            return;
        };
        self.bus.publish_lazy(|| {
            Event::new(EventKind::TaskRemoveRequested)
                .with_task(Arc::clone(&label))
                .with_id(id)
                .with_reason("manual_cancel")
        });
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
            self.spawn_join_report(id, handle, Some(self.grace), completion);
        } else {
            let _ = reply.send(Ok(Some(decision)));
        }
    }

    /// Cleans up a finished actor by identity.
    ///
    /// Called after the actor's reliable completion signal is received.
    /// Duplicate or stale completion signals are no-ops.
    pub(super) async fn cleanup_completed_task(&self, id: TaskId) {
        let Some((_label, mut handle, removal_completion)) = self.claim_task(id).await else {
            return;
        };
        if !handle.result_ready() {
            // Completion identities are internal and normally follow the result
            // send. Keep the listener responsive if a defensive early signal is
            // nevertheless observed.
            self.spawn_join_report(id, handle, Some(self.grace), removal_completion);
            return;
        }
        // The wrapper publishes the result before the completion id. Its tail
        // contains no user values, so collecting it here is immediate in the
        // genuine path and avoids a detached task, timer, or reaper record.
        let collected = handle.join_mut().await;
        let (done, cleanup) = handle.into_report_parts();
        let reaper = self.actors.attempt_reaper();
        Self::finish_removal(
            &self.state,
            &self.empty_notify,
            &self.pending_joins,
            &self.bus,
            &reaper,
            RemovalReport {
                id,
                outcome: done,
                join: JoinCompletion::Joined(collected),
                completion: removal_completion,
                cleanup,
            },
        )
        .await;
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
        pending_joins.inc_with_label(id, Arc::clone(&label));
        Some((label, *handle, completion))
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
        handle: Handle,
        force_after: Option<Duration>,
        removal_completion: RemovalCompletion,
    ) {
        let bus = self.bus.clone();
        let state = Arc::clone(&self.state);
        let empty_notify = Arc::clone(&self.empty_notify);
        let pending = Arc::clone(&self.pending_joins);
        let reaper = self.actors.attempt_reaper();
        let runtime_token = self.runtime_token.clone();
        tokio::spawn(async move {
            let mut handle = handle;
            let completion = match force_after {
                Some(grace) => {
                    tokio::select! {
                        biased;
                        res = handle.join_mut() => JoinCompletion::Joined(res),
                        _ = runtime_token.cancelled() => {
                            handle.abort();
                            let _ = handle.join_mut().await;
                            JoinCompletion::ForceAborted
                        }
                        _ = tokio::time::sleep(grace) => {
                            handle.abort();
                            let _ = handle.join_mut().await;
                            JoinCompletion::ForceAborted
                        }
                    }
                }
                None => {
                    tokio::select! {
                        biased;
                        res = handle.join_mut() => JoinCompletion::Joined(res),
                        _ = runtime_token.cancelled() => {
                            handle.abort();
                            let _ = handle.join_mut().await;
                            JoinCompletion::ForceAborted
                        }
                    }
                }
            };
            let (done, cleanup) = handle.into_report_parts();

            Self::finish_removal(
                &state,
                &empty_notify,
                &pending,
                &bus,
                &reaper,
                RemovalReport {
                    id,
                    outcome: done,
                    join: completion,
                    completion: removal_completion,
                    cleanup,
                },
            )
            .await;
        });
    }

    /// Commits terminal cleanup for one `Removing` entry.
    ///
    /// State removal, outcome delivery, terminal events, and pending-join cleanup finish before an empty-registry waiter can continue.
    pub(super) async fn finish_removal(
        state: &Arc<RwLock<Inner>>,
        empty_notify: &Arc<Notify>,
        pending_joins: &Arc<PendingJoins>,
        bus: &Bus,
        reaper: &AttemptReaper,
        report: RemovalReport,
    ) {
        let RemovalReport {
            id,
            outcome,
            join,
            completion: removal_completion,
            mut cleanup,
        } = report;
        let terminal_outcome = match join {
            JoinCompletion::Joined(result) => Self::outcome_of(result, &mut cleanup),
            JoinCompletion::ForceAborted => TaskOutcome::ForceAborted,
        };
        let mut pending_report = PendingTerminalReport {
            state: Arc::clone(state),
            empty_notify: Arc::clone(empty_notify),
            pending_joins: Arc::clone(pending_joins),
            bus: bus.clone(),
            id,
            outcome: Some(terminal_outcome),
            done: outcome,
            cleanup: Some(cleanup),
            reaper: reaper.clone(),
            completion: removal_completion,
            detach_on_drop: true,
        };
        pending_report.commit().await;
    }

    /// Publishes one typed terminal event, delivers the watched outcome, then publishes registry removal.
    ///
    /// The event and waiter are classified from the same [`TaskOutcome`] value.
    /// `reason` remains diagnostic; callers branch on [`TaskOutcomeKind`](crate::TaskOutcomeKind).
    fn report_outcome(
        bus: &Bus,
        id: TaskId,
        name: &str,
        outcome: TaskOutcome,
        done: Option<OutcomeTx>,
        cleanup: &mut DropBundle,
    ) {
        let mut outcome = OutcomeDropGuard::new(outcome, cleanup);
        bus.publish_lazy(|| {
            let mut finished = Event::new(EventKind::TaskFinished)
                .with_task(name)
                .with_id(id)
                .with_outcome_kind(outcome.get().kind());
            match outcome.get() {
                TaskOutcome::Failed {
                    reason, exit_code, ..
                }
                | TaskOutcome::Fatal {
                    reason, exit_code, ..
                } => {
                    finished = finished.with_reason(Arc::clone(reason));
                    if let Some(code) = exit_code {
                        finished = finished.with_exit_code(*code);
                    }
                }
                TaskOutcome::ForceAborted => {
                    finished =
                        finished.with_reason("task did not stop within grace; force-aborted");
                }
                TaskOutcome::Panicked => {
                    finished = finished.with_reason("internal task runner panicked");
                }
                TaskOutcome::Completed | TaskOutcome::Canceled | TaskOutcome::Rejected { .. } => {}
            }
            finished
        });
        if let Some(done) = done
            && let Err(undelivered) = done.send(outcome.take())
        {
            outcome.cleanup.attach_outcome(undelivered);
        }
        bus.publish_lazy(|| {
            Event::new(EventKind::TaskRemoved)
                .with_task(name)
                .with_id(id)
        });
    }

    /// Maps a joined actor result to the public [`TaskOutcome`].
    fn outcome_of(
        res: Result<ActorExitReason, ActorJoinError>,
        cleanup: &mut DropBundle,
    ) -> TaskOutcome {
        match res {
            Ok(ActorExitReason::Completed) => TaskOutcome::Completed,
            Ok(ActorExitReason::Canceled) => TaskOutcome::Canceled,
            Ok(ActorExitReason::Panicked { cleanup_poisoned }) => {
                if cleanup_poisoned {
                    cleanup.poison();
                }
                TaskOutcome::Panicked
            }
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
            Err(e) if e.is_panic() => {
                if e.cleanup_poisoned() {
                    cleanup.poison();
                }
                TaskOutcome::Panicked
            }
            Err(_aborted) => TaskOutcome::ForceAborted,
        }
    }
}
