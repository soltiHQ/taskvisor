//! Commits final registry state after the actor join produces a result.
//!
//! Every removal source reaches this module with one [`RemovalReport`]. The join result is first
//! mapped to its public outcome. Terminal commit then removes the identity and name indexes under
//! the state lock. After the lock is released, it publishes terminal events, resolves a watched outcome,
//! and transfers user values with their capacity reservation to physical and deferred cleanup.
//!
//! Logical completion, pending-join release, and empty-registry notification form a guarded tail.
//! They still run if reporting unwinds. If a future waiting for the state lock is cancelled,
//! its report moves to a detached continuation when a Tokio runtime is available.
//! No user-owned terminal value is dropped on the listener or actor task by this module.

use std::sync::Arc;

use tokio::sync::{Notify, RwLock};

use super::{JoinCompletion, PendingJoins, RemovalReport};
use crate::{
    core::{
        actor::ActorExitReason,
        deferred_drop::DropBundle,
        outcome::TaskOutcome,
        registry::{
            Registry,
            completion::{OutcomeTx, RemovalCompletion},
            scheduler::{ActorJoinError, AttemptReaper},
            state::{EntryState, Inner},
        },
    },
    events::{Bus, Event, EventKind},
    identity::TaskId,
};

/// Keeps an undelivered outcome on the isolated destruction path.
///
/// The guard retains ownership if event publication or outcome delivery unwinds.
struct OutcomeDropGuard<'a> {
    /// Outcome retained until its watched channel accepts ownership.
    outcome: Option<TaskOutcome>,
    /// Bundle used when the outcome cannot be delivered.
    cleanup: &'a mut DropBundle,
}

impl<'a> OutcomeDropGuard<'a> {
    /// Creates a guard for one classified terminal outcome.
    fn new(outcome: TaskOutcome, cleanup: &'a mut DropBundle) -> Self {
        Self {
            outcome: Some(outcome),
            cleanup,
        }
    }

    /// Borrows the outcome while the guard retains ownership.
    fn get(&self) -> &TaskOutcome {
        self.outcome
            .as_ref()
            .expect("the terminal outcome remains guarded until delivery")
    }

    /// Transfers the outcome to its watched channel at most once.
    fn take(&mut self) -> TaskOutcome {
        self.outcome
            .take()
            .expect("the terminal outcome is delivered at most once")
    }
}

impl Drop for OutcomeDropGuard<'_> {
    /// Moves an undelivered outcome into the cleanup bundle.
    fn drop(&mut self) {
        if let Some(outcome) = self.outcome.take() {
            self.cleanup.attach_outcome(outcome);
        }
    }
}

/// Owns one terminal report until registry membership is removed.
///
/// Cancellation while waiting for the state lock can spawn one detached commit.
/// Physical ownership remains retained if no runtime can run that continuation.
struct PendingTerminalReport {
    /// Authoritative membership state.
    state: Arc<RwLock<Inner>>,
    /// Notification used when terminal removal empties the registry.
    empty_notify: Arc<Notify>,
    /// Barrier for every claimed removal owner.
    pending_joins: Arc<PendingJoins>,
    /// Event destination for terminal diagnostics.
    bus: Bus,
    /// Identity whose removing entry must be committed.
    id: TaskId,
    /// Classified terminal outcome awaiting delivery.
    outcome: Option<TaskOutcome>,
    /// Optional watched outcome sender.
    done: Option<OutcomeTx>,
    /// User values and reserved capacity awaiting isolated destruction.
    cleanup: Option<DropBundle>,
    /// Physical ownership sink for terminal values.
    reaper: AttemptReaper,
    /// Two-phase completion for this removal.
    completion: RemovalCompletion,
    /// Whether cancellation should spawn one commit continuation.
    detach_on_drop: bool,
}

impl PendingTerminalReport {
    /// Takes the outcome, sender, and cleanup bundle for terminal reporting.
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

    /// Removes membership and commits terminal side effects without another await.
    ///
    /// Completion waiters wake only after both indexes are updated, reporting is attempted, and the state lock is released.
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
                if st.by_name.get(entry.name.as_ref()) == Some(&self.id) {
                    st.by_name.remove(entry.name.as_ref());
                }
                let is_empty = st.tasks.is_empty();
                Some((entry.name, state_completion, is_empty))
            }
        };

        let Some((name, state_completion, is_empty)) = removed else {
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
            &name,
            terminal_outcome,
            outcome,
            cleanup,
        );
        drop(finalizer);
    }

    /// Completes a stale report after the state lock confirms missing membership.
    fn finish_without_membership(&mut self) {
        if self.retain_terminal() {
            self.pending_joins.dec(self.id);
            self.completion.complete_logical();
        }
    }

    /// Transfers terminal ownership without completing registry barriers.
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

    /// Moves an interrupted commit into a non-detaching continuation guard.
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

    /// Retains physical ownership when a detached commit cannot finish.
    ///
    /// Membership and logical completion remain pending because no state commit occurred.
    fn retain_without_logical_completion(&mut self) {
        let _ = self.retain_terminal();
    }
}

impl Drop for PendingTerminalReport {
    /// Continues an interrupted commit or retains its physical ownership.
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

/// Completes the terminal tail even if outcome reporting unwinds.
pub(in crate::core::registry) struct TerminalFinalizer<'a> {
    /// Identity whose terminal ownership is released.
    pub(in crate::core::registry) id: TaskId,
    /// Notification used when removal empties the registry.
    pub(in crate::core::registry) empty_notify: &'a Notify,
    /// Join-owner barrier decremented by this finalizer.
    pub(in crate::core::registry) pending_joins: &'a PendingJoins,
    /// Completion stored in the authoritative removing entry.
    pub(in crate::core::registry) state_completion: Option<RemovalCompletion>,
    /// Completion carried by the terminal report.
    pub(in crate::core::registry) report_completion: RemovalCompletion,
    /// Whether membership removal left the registry empty.
    pub(in crate::core::registry) is_empty: bool,
    /// Reaper and cleanup bundle that retain physical ownership.
    pub(in crate::core::registry) terminal: Option<(AttemptReaper, DropBundle)>,
}

impl Drop for TerminalFinalizer<'_> {
    /// Transfers terminal ownership and completes every logical barrier.
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
    /// Commits terminal cleanup for one removing entry.
    ///
    /// Membership removal, reporting, and pending-join accounting finish before an empty-registry waiter can continue.
    pub(in crate::core::registry) async fn finish_removal(
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

    /// Publishes terminal events and delivers the watched outcome.
    ///
    /// The event and waiter use the same [`TaskOutcome`] classification.
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

    /// Maps an actor join result to its public terminal outcome.
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
