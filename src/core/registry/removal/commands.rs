//! Resolves remove and cancel commands against authoritative state.
//!
//! The registry listener calls this module after a management command reaches its
//! ordering point. Identity commands inspect one entry. Label commands resolve the
//! current identity and make the claim under the same state lock.
//!
//! Remove returns only whether this command won the actor handle. It does not
//! wait for terminal cleanup.
//! Cancel returns the logical completion latch. A later cancel of an entry already being removed
//! joins the same completion instead of creating another join owner.

use std::sync::Arc;

use tokio::sync::oneshot;

use super::PendingJoins;
use crate::{
    core::registry::{
        Registry,
        protocol::{CancelDecision, CancelReply, RemoveReply},
        state::{EntryState, Handle, Inner},
    },
    events::{Event, EventKind},
    identity::TaskId,
};

/// Registry work selected while one cancel command holds the state lock.
struct CancelAction {
    /// Decision returned to the command caller.
    decision: CancelDecision,
    /// Actor handle owned when this command won the removal claim.
    handle: Option<Handle>,
}

impl Registry {
    /// Removes a task by identity.
    ///
    /// `Ok(true)` means this command claimed the actor and triggered cancellation.
    /// Membership remains until terminal join cleanup.
    ///
    /// `Ok(false)` means the entry is unknown or already claimed.
    /// An existing owner can still publish `TaskRemoved` later.
    pub(in crate::core::registry) async fn remove_task(
        &self,
        id: TaskId,
        reply: oneshot::Sender<RemoveReply>,
    ) {
        if let Some((_label, handle, completion)) = self.claim_task(id).await {
            handle.cancel.cancel();
            let _ = reply.send(Ok(true));
            self.spawn_join_report(id, handle, Some(self.grace), completion);
        } else {
            let _ = reply.send(Ok(false));
        }
    }

    /// Resolves a label and claims its current owner under one state lock.
    ///
    /// A missing label returns `Ok(false)` without a request event.
    /// An entry already being removed gets another request event and returns `Ok(false)`.
    pub(in crate::core::registry) async fn remove_task_by_label(
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

    /// Claims or joins cancellation by identity and returns its terminal decision.
    pub(in crate::core::registry) async fn cancel_task(
        &self,
        id: TaskId,
        reply: oneshot::Sender<CancelReply>,
    ) {
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

    /// Resolves a label and claims or joins cancellation under one state lock.
    pub(in crate::core::registry) async fn cancel_task_by_label(
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

    /// Sends a cancel decision and starts its join owner when this command claimed it.
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
}
