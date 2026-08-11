//! Runtime startup and static-run lifecycle.

use std::{
    future::Future,
    sync::{Arc, atomic::Ordering},
};

use super::{SupervisorCore, shutdown_workflow::ShutdownTrigger};
use crate::{core::registry::AddBatchItem, error::RuntimeError, identity::TaskId, tasks::TaskSpec};

impl SupervisorCore {
    /// Starts runtime workers and listeners.
    ///
    /// This starts:
    /// - subscriber queue workers,
    /// - the event relay,
    /// - the registry listener and central actor scheduler.
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
        self.subscriber_listener();
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
        if self.running.swap(true, Ordering::AcqRel) {
            return Err(RuntimeError::AlreadyRunning);
        }
        if self.is_shutting_down() {
            return self.wait_started_shutdown().await;
        }
        self.start();

        if tasks.is_empty() {
            return self.drive_shutdown(shutdown).await;
        }

        let items = tasks
            .into_iter()
            .map(|spec| AddBatchItem {
                id: TaskId::next(),
                label: Arc::from(spec.task().name()),
                spec,
            })
            .collect();
        let reply = match self.enqueue_add_batch_wait(items).await {
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
