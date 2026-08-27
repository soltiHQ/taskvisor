//! Commits a static task batch as one registry decision.
//!
//! Static run sends one [`RegistryCommand::AddBatch`](crate::core::registry::RegistryCommand::AddBatch) to the listener.
//! This module rejects duplicate names inside the batch and conflicts with registry ownership or force-aborted attempts that have not physically exited.
//! It also checks the registration limit for the complete batch.
//!
//! Validation runs before actor preparation and again under the state write lock.
//! Rejection inserts no entries and starts no task bodies.
//! Acceptance indexes every item first.
//! It then attempts every `TaskAdded` event and the direct batch reply before spawning the actors and opening their shared gate.

use std::{collections::HashSet, sync::Arc};

use tokio::sync::{oneshot, watch};

use super::PreparedRegistration;
use crate::{
    core::registry::{
        Registry,
        protocol::{AddBatchItem, AddReply},
        state::{Entry, EntryState, Handle, HandleCleanup},
    },
    error::RuntimeError,
    events::{Event, EventKind, RejectionKind},
    reasons,
};

impl Registry {
    /// All-or-nothing decision for one batch command.
    pub(in crate::core::registry) async fn spawn_and_register_batch(
        &self,
        items: Vec<AddBatchItem>,
        reply: oneshot::Sender<AddReply>,
    ) {
        let reaper = self.actors.attempt_reaper();
        let mut seen = HashSet::with_capacity(items.len());
        let mut conflicting_ids = HashSet::new();
        let mut first_conflict = None;
        let current = {
            let st = self.state.read().await;
            let reaper_conflicts =
                reaper.reserves_names(items.iter().map(|item| item.name.as_ref()));
            for (item, reaper_conflict) in items.iter().zip(reaper_conflicts) {
                let conflicts_with_registry =
                    st.by_name.contains_key(&item.name) || reaper_conflict;
                let repeats_in_batch = !seen.insert(item.name.as_ref());
                if conflicts_with_registry || repeats_in_batch {
                    first_conflict.get_or_insert_with(|| Arc::clone(&item.name));
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
                        .with_task(Arc::clone(&item.name))
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
                        .with_task(Arc::clone(&item.name))
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
                    item.name,
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
            reaper.reserves_names(prepared.iter().map(|item| item.name.as_ref()));
        for (item, reaper_conflict) in prepared.iter().zip(reaper_conflicts) {
            let conflicts_with_registry = st.by_name.contains_key(&item.name) || reaper_conflict;
            let repeats_in_batch = !seen.insert(item.name.as_ref());
            if conflicts_with_registry || repeats_in_batch {
                first_conflict.get_or_insert_with(|| Arc::clone(&item.name));
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
                        .with_task(Arc::clone(&item.name))
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
                        .with_task(Arc::clone(&item.name))
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
                name,
                join,
                cancel,
                done,
                completion,
                scheduled,
                cleanup,
                activity,
            } = item;
            let entry = Entry {
                name: Arc::clone(&name),
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
            st.by_name.insert(Arc::clone(&name), id);
            accepted.push((id, name, scheduled));
        }
        drop(st);

        for (id, name, _) in &accepted {
            self.bus.publish_lazy(|| {
                Event::new(EventKind::TaskAdded)
                    .with_task(Arc::clone(name))
                    .with_id(*id)
            });
        }
        let _ = reply.send(Ok(()));
        self.actors
            .schedule_batch(accepted.into_iter().map(|(_, _, scheduled)| scheduled));
        start_tx.send_replace(true);
    }
}
