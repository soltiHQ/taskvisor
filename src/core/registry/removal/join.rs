//! Joins claimed actors and hands their results to terminal cleanup.
//!
//! Command handlers and completion cleanup use this module after a successful
//! registered-to-removing transition. The actor handle leaves shared state before any wait.
//! A ready natural result can finish inline. Other joins run in detached reporters.
//! Every result returns through [`Registry::finish_removal`].
//!
//! Shutdown also claims entries here. [`PendingJoins`] provides its barrier and diagnostic labels.

use std::{sync::Arc, time::Duration};

use super::{JoinCompletion, PendingJoins, RemovalReport};
use crate::{
    core::registry::{
        Registry,
        completion::RemovalCompletion,
        state::{EntryState, Handle, Inner},
    },
    identity::TaskId,
};

impl Registry {
    /// Waits up to `grace` for all claimed removal owners.
    ///
    /// An owner finishes after membership removal, outcome delivery, and the final `TaskRemoved`
    /// publication are attempted. This barrier is separate from cancellation completion.
    ///
    /// Returns labels for removal owners still active when `grace` expires.
    pub async fn wait_joins_within(&self, grace: Duration) -> Vec<Arc<str>> {
        let _ = tokio::time::timeout(grace, self.pending_joins.wait_drained()).await;
        self.pending_joins.pending_labels()
    }

    /// Claims and cancels all registered entries within one shared grace window.
    ///
    /// Entries already being removed keep their owner. This method waits for all
    /// pending removal owners only until the same deadline.
    ///
    /// Returns labels for actors claimed here that required force-abort.
    /// [`wait_joins_within`](Self::wait_joins_within) reports older owners.
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

    /// Cleans up a finished actor by identity.
    ///
    /// Duplicate or stale completion signals are no-ops. An early signal starts
    /// a bounded detached join.
    /// A ready result is collected inline because the actor tail no longer owns user values.
    pub(in crate::core::registry) async fn cleanup_completed_task(&self, id: TaskId) {
        let Some((_label, mut handle, removal_completion)) = self.claim_task(id).await else {
            return;
        };
        if !handle.result_ready() {
            self.spawn_join_report(id, handle, Some(self.grace), removal_completion);
            return;
        }
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

    /// Changes one task from registered to removing state.
    ///
    /// The winning caller receives the only actor handle.
    /// Identity and label indexes remain until that caller finishes the join.
    pub(super) async fn claim_task(
        &self,
        id: TaskId,
    ) -> Option<(Arc<str>, Handle, RemovalCompletion)> {
        let mut st = self.state.write().await;
        Self::claim_registered(&mut st, &self.pending_joins, id)
    }

    /// Performs the registered-to-removing transition under the state lock.
    pub(super) fn claim_registered(
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

    /// Joins an actor in a detached task and commits its terminal result.
    ///
    /// `force_after` bounds the join when present.
    /// Runtime shutdown always aborts an unfinished actor.
    /// Both paths commit through `finish_removal`.
    pub(super) fn spawn_join_report(
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
}
