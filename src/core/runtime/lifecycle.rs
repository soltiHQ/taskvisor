//! Runtime startup and static-run lifecycle.

use std::{
    future::Future,
    sync::{Arc, atomic::Ordering},
};

use super::{SupervisorCore, shutdown_workflow::ShutdownTrigger};
use crate::{
    core::{
        deferred_drop,
        registry::AddBatchItem,
        task_metadata::{self, TaskNameSnapshot},
    },
    error::RuntimeError,
    identity::TaskId,
    tasks::TaskSpec,
};

impl SupervisorCore {
    /// Starts runtime workers and listeners.
    ///
    /// This starts:
    /// - subscriber queue workers,
    /// - the event relay,
    /// - the registry listener and actor runtime/reaper coordinator.
    pub(crate) fn start(&self) {
        if self.started.load(Ordering::Acquire) {
            return;
        }

        let _startup = self
            .startup_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.started.load(Ordering::Acquire) {
            return;
        }

        tokio::runtime::Handle::try_current()
            .expect("Supervisor::serve requires an active Tokio runtime");
        self.subs.start();
        if self.bus.is_enabled() {
            self.subscriber_listener();
        }
        self.registry.clone().spawn_listener();
        self.started.store(true, Ordering::Release);
    }

    /// Runs a static task set until shared shutdown or registry emptiness.
    ///
    /// This starts the runtime and, when the task set is non-empty, registers it as one atomic batch.
    /// It then drives shutdown or natural completion.
    ///
    /// Single-shot: a second or concurrent call returns [`RuntimeError::AlreadyRunning`].
    pub(crate) async fn run(self: &Arc<Self>, tasks: Vec<TaskSpec>) -> Result<(), RuntimeError> {
        self.run_until_trigger(tasks, std::future::pending()).await
    }

    /// Runs a static task set with one explicit external shutdown trigger.
    pub(crate) async fn run_until<F>(
        self: &Arc<Self>,
        tasks: Vec<TaskSpec>,
        shutdown: F,
    ) -> Result<(), RuntimeError>
    where
        F: Future<Output = ()>,
    {
        self.run_until_trigger(tasks, async move {
            shutdown.await;
            ShutdownTrigger::Requested
        })
        .await
    }

    /// Runs a static task set with explicit process-signal handling.
    pub(crate) async fn run_with_os_signals(
        self: &Arc<Self>,
        tasks: Vec<TaskSpec>,
    ) -> Result<(), RuntimeError> {
        self.run_until_trigger(tasks, async {
            match crate::core::shutdown::wait_for_shutdown_signal().await {
                Ok(()) => ShutdownTrigger::Requested,
                Err(source) => ShutdownTrigger::SignalSetupFailed(Arc::new(source)),
            }
        })
        .await
    }

    /// Owns the single-shot static lifecycle for every shutdown-source variant.
    async fn run_until_trigger<F>(
        self: &Arc<Self>,
        tasks: Vec<TaskSpec>,
        shutdown: F,
    ) -> Result<(), RuntimeError>
    where
        F: Future<Output = ShutdownTrigger>,
    {
        if self.running.load(Ordering::Acquire) {
            return Err(RuntimeError::AlreadyRunning);
        }

        if let Some(limit) = self.runtime_config().max_registered_tasks()
            && tasks.len() > limit.get()
        {
            return Err(RuntimeError::ResourceLimitReached {
                resource: "registered_tasks",
                limit: limit.get(),
            });
        }

        // Subscriber reservations live for this supervisor's whole lifetime.
        // A static batch that cannot fit beside them can never acquire the
        // process-wide ownership broker, so reject before enqueueing an
        // impossible waiter that would also obstruct unrelated admissions.
        if tasks.len()
            > deferred_drop::OWNERSHIP_CAPACITY.saturating_sub(self.subs.ownership_slots())
        {
            return Err(RuntimeError::ResourceLimitReached {
                resource: deferred_drop::OWNERSHIP_RESOURCE,
                limit: deferred_drop::OWNERSHIP_CAPACITY,
            });
        }

        if tasks.is_empty() {
            if self
                .running
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return Err(RuntimeError::AlreadyRunning);
            }
            if self.is_shutting_down() {
                return self.wait_started_shutdown().await;
            }
            self.start();
            return self.drive_shutdown(shutdown).await;
        }

        tokio::pin!(shutdown);
        tokio::select! {
            biased;
            _ = self.shutdown.started.cancelled() => {
                return self.wait_started_shutdown().await;
            }
            trigger = shutdown.as_mut() => {
                if self.running.compare_exchange(
                    false,
                    true,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ).is_err() {
                    return Err(RuntimeError::AlreadyRunning);
                }
                self.start();
                return self.join_shutdown(trigger).await;
            }
            _ = std::future::ready(()) => {}
        }
        let reservations = self
            .try_reserve_ownership_many(tasks.len())
            .map_err(|error| RuntimeError::ResourceLimitReached {
                resource: deferred_drop::OWNERSHIP_RESOURCE,
                limit: error.limit(),
            })?;

        // Charge every task before handing synchronous user metadata to the
        // process-wide fixed worker set. A name panic still occurs before the
        // single-shot lifecycle is consumed, so a corrected batch may retry.
        let owned_tasks: Vec<_> = tasks
            .into_iter()
            .zip(reservations)
            .map(|(spec, reservation)| self.own_task(spec, reservation))
            .collect();
        let mut metadata = Vec::with_capacity(owned_tasks.len());
        for owned in owned_tasks {
            let receiver = match task_metadata::snapshot_task_name(owned, |spec| {
                Arc::<str>::from(spec.task().name())
            }) {
                Ok(receiver) => receiver,
                Err(owned) => {
                    drop(owned);
                    return Err(RuntimeError::ResourceLimitReached {
                        resource: "task_metadata",
                        limit: crate::core::deferred_drop::OWNERSHIP_CAPACITY,
                    });
                }
            };
            metadata.push(receiver);
        }

        let mut tasks = Vec::with_capacity(metadata.len());
        for receiver in metadata {
            let snapshot = tokio::select! {
                biased;
                _ = self.shutdown.started.cancelled() => {
                    return self.wait_started_shutdown().await;
                }
                trigger = shutdown.as_mut() => {
                    if self.running.compare_exchange(
                        false,
                        true,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ).is_err() {
                        return Err(RuntimeError::AlreadyRunning);
                    }
                    self.start();
                    return self.join_shutdown(trigger).await;
                }
                snapshot = receiver => snapshot.map_err(|_| RuntimeError::ResourceLimitReached {
                    resource: "task_metadata",
                    limit: crate::core::deferred_drop::OWNERSHIP_CAPACITY,
                })?,
            };
            match snapshot {
                TaskNameSnapshot::Ready { owned, task_name } => {
                    tasks.push((task_name, owned));
                }
                TaskNameSnapshot::Panicked { owned, message } => {
                    drop(owned);
                    panic!("Task::name panicked: {message}")
                }
            }
        }

        if self
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(RuntimeError::AlreadyRunning);
        }
        if self.is_shutting_down() {
            return self.wait_started_shutdown().await;
        }
        self.start();

        let items = tasks
            .into_iter()
            .map(|(label, owned)| AddBatchItem {
                id: TaskId::next(),
                label,
                owned,
            })
            .collect();
        let admission = tokio::select! {
            biased;
            _ = self.shutdown.started.cancelled() => {
                return self.wait_started_shutdown().await;
            }
            trigger = shutdown.as_mut() => {
                return self.join_shutdown(trigger).await;
            }
            admission = self.enqueue_add_batch_wait(items) => admission,
        };
        let reply = match admission {
            Ok(reply) => reply,
            Err(RuntimeError::ShuttingDown) if self.shutdown.started.is_cancelled() => {
                return self.wait_started_shutdown().await;
            }
            Err(error) => return Err(error),
        };

        match Self::await_add_batch_reply(reply).await {
            Ok(()) => self.drive_shutdown(shutdown).await,
            Err(RuntimeError::ShuttingDown) if self.shutdown.started.is_cancelled() => {
                self.wait_started_shutdown().await
            }
            Err(error) => Err(error),
        }
    }

    /// Drives static-mode completion.
    ///
    /// Waits for either:
    /// - a shared shutdown already started by another entry point,
    /// - the caller-provided shutdown trigger,
    /// - natural completion when the registry becomes empty.
    ///
    /// Every path joins registry and subscriber listeners and closes subscribers before returning.
    async fn drive_shutdown<F>(self: &Arc<Self>, shutdown: F) -> Result<(), RuntimeError>
    where
        F: Future<Output = ShutdownTrigger>,
    {
        tokio::select! {
            _ = self.shutdown.started.cancelled() => self.wait_started_shutdown().await,
            trigger = shutdown => self.join_shutdown(trigger).await,
            _ = self.registry.wait_until_empty() => self.join_shutdown(ShutdownTrigger::Natural).await,
        }
    }
}
