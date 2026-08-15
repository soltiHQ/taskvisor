//! Commits one add command or returns its rejection.
//!
//! The registry listener calls this module after it receives [`RegistryCommand::Add`](crate::core::registry::RegistryCommand::Add).
//! Label conflicts include current membership and force-aborted attempts that have not physically exited.
//! The configured registration limit includes both groups.
//!
//! Validation runs before actor preparation and again under the state write lock.
//! On success, both indexes are updated before the `TaskAdded` event, the direct reply, actor spawn, and start-gate release.
//! On rejection, no task body starts. A watched rejection is delivered or moved to deferred cleanup.

use std::sync::Arc;

use tokio::sync::{oneshot, watch};

use super::{PreparedRegistration, deliver_or_attach_rejection};
use crate::{
    core::{
        deferred_drop::OwnedTask,
        outcome::TaskOutcome,
        registry::{
            Registry,
            completion::{OutcomeTx, RemovalCompletion},
            protocol::AddReply,
            state::{Entry, EntryState, Handle, HandleCleanup},
        },
    },
    error::RuntimeError,
    events::{Event, EventKind, RejectionKind},
    identity::TaskId,
    reasons,
    tasks::TaskSpec,
};

impl Registry {
    /// Applies the authoritative decision for one add command.
    pub(in crate::core::registry) async fn spawn_and_register(
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
}
